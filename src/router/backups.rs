use super::ServerRef;
use crate::backup::{self, Adapter, valid_backup_uuid};
use crate::context::AppRef;
use crate::error::{ApiError, ApiJson, ApiQuery, ApiResult};
use crate::jwt::BackupDownloadClaims;
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::Deserialize;
use crate::protocol::wings_api::{BackupRequest as CreateRequest, RestoreRequest};
use serde_json::{Value, json};
use std::collections::HashMap;

pub fn routes() -> Router<AppRef> {
    Router::new()
        .route("/", post(create))
        .route("/{backup}/restore", post(restore))
        .route("/{backup}", delete(remove))
}

pub fn public_routes() -> Router<AppRef> {
    Router::new().route("/download/backup", get(download))
}

fn backup_param(params: &HashMap<String, String>) -> ApiResult<String> {
    let id = params.get("backup").cloned().unwrap_or_default();
    if !valid_backup_uuid(&id) {
        return Err(ApiError::bad_request("invalid backup uuid"));
    }
    Ok(id)
}


async fn create(ServerRef(s): ServerRef, ApiJson(req): ApiJson<CreateRequest>) -> ApiResult<(StatusCode, Json<Value>)> {
    if !valid_backup_uuid(&req.uuid) {
        return Err(ApiError::bad_request("invalid backup uuid"));
    }
    if s.is_restoring() || s.is_transferring() {
        return Err(ApiError::conflict("server is busy"));
    }
    tokio::spawn(backup::create(s.clone(), req.adapter, req.uuid.clone(), req.ignore));
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "status": "accepted", "server_id": s.uuid(), "backup": req.uuid })),
    ))
}


async fn restore(
    ServerRef(s): ServerRef,
    Path(params): Path<HashMap<String, String>>,
    ApiJson(req): ApiJson<RestoreRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let backup = backup_param(&params)?;
    s.ensure_idle()?;
    if s.state() != crate::server::events::PowerState::Offline {
        return Err(ApiError::conflict("server must be offline to restore a backup"));
    }
    if req.adapter == Adapter::Wings && !backup::local_path(&s, &backup).exists() {
        return Err(ApiError::not_found("backup not found on this node"));
    }
    if req.adapter == Adapter::S3 && req.download_url.is_none() {
        return Err(ApiError::bad_request("download_url is required for s3 backups"));
    }
    s.set_restoring(true);
    tokio::spawn(backup::restore(
        s.clone(),
        req.adapter,
        backup.clone(),
        req.truncate_directory,
        req.download_url,
    ));
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "status": "accepted", "server_id": s.uuid(), "backup": backup })),
    ))
}

async fn remove(
    ServerRef(s): ServerRef,
    Path(params): Path<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let backup = backup_param(&params)?;
    let path = backup::local_path(&s, &backup);
    match tokio::fs::remove_file(&path).await {
        Ok(()) => Ok(Json(json!({ "status": "ok", "backup": backup }))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(ApiError::not_found("backup not found")),
        Err(e) => Err(anyhow::Error::from(e).into()),
    }
}

#[derive(Deserialize)]
struct TokenQuery {
    token: String,
}

async fn download(State(app): State<AppRef>, ApiQuery(q): ApiQuery<TokenQuery>) -> ApiResult<Response> {
    let claims: BackupDownloadClaims = app
        .jwt
        .decode(&q.token)
        .map_err(|_| ApiError::unauthorized("invalid or expired token"))?;
    if !valid_backup_uuid(&claims.backup_uuid) {
        return Err(ApiError::bad_request("invalid backup uuid"));
    }
    if !app.jwt.consume_once(&claims.unique_id, claims.base.exp) {
        return Err(ApiError::forbidden("this download link has already been used"));
    }
    let s = app
        .manager
        .get(&claims.server_uuid)
        .ok_or_else(|| ApiError::not_found("server not found"))?;
    let path = backup::local_path(&s, &claims.backup_uuid);
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| ApiError::not_found("backup not found"))?;
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let mut res = Body::from_stream(tokio_util::io::ReaderStream::new(file)).into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/gzip"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{}.tar.gz\"", claims.backup_uuid)) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(res)
}
