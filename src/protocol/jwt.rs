use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaseClaims {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jti: Option<String>,
    pub exp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iat: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nbf: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebsocketClaims {
    #[serde(flatten)]
    pub base: BaseClaims,
    pub user_uuid: String,
    pub server_uuid: String,
    #[serde(default)]
    pub permissions: Vec<String>,
}

impl WebsocketClaims {
    pub fn has_permission(&self, perm: &str) -> bool {
        self.permissions.iter().any(|p| p == "*" || p == perm)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDownloadClaims {
    #[serde(flatten)]
    pub base: BaseClaims,
    pub server_uuid: String,
    pub file_path: String,
    pub unique_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupDownloadClaims {
    #[serde(flatten)]
    pub base: BaseClaims,
    pub server_uuid: String,
    pub backup_uuid: String,
    pub unique_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadClaims {
    #[serde(flatten)]
    pub base: BaseClaims,
    pub server_uuid: String,
    pub user_uuid: String,
    pub unique_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferClaims {
    #[serde(flatten)]
    pub base: BaseClaims,
    pub sub: String,
}
