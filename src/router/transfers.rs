use super::ServerRef;
use crate::backup::HashingWriter;
use crate::context::AppRef;
use crate::error::{ApiError, ApiJson, ApiResult};
use crate::filesystem::archive::{extract_tar_gz, write_tar_gz};
use crate::jwt::TransferClaims;
use crate::server::Server;
use crate::server::events::{PowerAction, ServerEvent};
use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use futures_util::TryStreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

static OUTGOING: LazyLock<Mutex<HashMap<String, tokio::task::AbortHandle>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn panel_routes() -> Router<AppRef> {
    Router::new().route(
        "/servers/{server}/transfer",
        post(start_outgoing).delete(cancel_outgoing),
    )
}

pub fn public_routes() -> Router<AppRef> {
    Router::new().route(
        "/api/transfers",
        post(incoming).layer(DefaultBodyLimit::disable()),
    )
}

fn log(server: &Server, msg: impl Into<String>) {
    let msg = msg.into();
    tracing::info!(server = %server.uuid(), "{msg}");
    server.publish(ServerEvent::TransferLogs(msg));
}

#[derive(Deserialize)]
struct OutgoingRequest {
    url: String,
    token: String,
}

async fn start_outgoing(
    ServerRef(s): ServerRef,
    ApiJson(req): ApiJson<OutgoingRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let url = reqwest::Url::parse(&req.url).map_err(|e| ApiError::bad_request(format!("invalid url: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ApiError::bad_request("url must be http(s)"));
    }
    s.ensure_idle()?;
    s.set_transferring(true);
    let server = s.clone();
    let task = tokio::spawn(async move {
        let uuid = server.uuid().to_string();
        let result = push(&server, url, &req.token).await;
        OUTGOING.lock().unwrap().remove(&uuid);
        match result {
            Ok(()) => {
                log(&server, "Transfer completed; waiting for the panel to remove the server from this node.");
                server.publish(ServerEvent::TransferStatus("completed".into()));
            }
            Err(e) => {
                tracing::error!(server = %uuid, error = ?e, "outgoing transfer failed");
                log(&server, format!("Transfer failed: {e}"));
                server.set_transferring(false);
                server.publish(ServerEvent::TransferStatus("failure".into()));
                if let Err(e) = server.ctx.remote.set_transfer_status(&uuid, false).await {
                    tracing::warn!(server = %uuid, error = %e, "failed to report transfer failure");
                }
            }
        }
    });
    OUTGOING
        .lock()
        .unwrap()
        .insert(s.uuid().to_string(), task.abort_handle());
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "status": "accepted", "server_id": s.uuid(), "action": "transfer" })),
    ))
}

async fn push(server: &Arc<Server>, url: reqwest::Url, token: &str) -> Result<()> {
    server.publish(ServerEvent::TransferStatus("processing".into()));
    if server.env.is_running().await.unwrap_or(false) {
        log(server, "Stopping server before transfer...");
        server.handle_power(PowerAction::Kill, None).await.ok();
        let mut rx = server.subscribe_state();
        let _ = tokio::time::timeout(
            Duration::from_secs(60),
            rx.wait_for(|s| *s == crate::server::events::PowerState::Offline),
        )
        .await;
    }

    log(server, "Creating archive of server files...");
    let tmp = server
        .ctx
        .config
        .system
        .tmp_directory
        .join(format!("{}-transfer.tar.gz", server.uuid()));
    let fs = server.fs.clone();
    let tmp2 = tmp.clone();
    let (checksum, size) = tokio::task::spawn_blocking(move || -> Result<(String, u64)> {
        let file = std::fs::File::create(&tmp2)?;
        let w = HashingWriter::new(std::io::BufWriter::new(file));
        let mut w = write_tar_gz(&fs, Path::new("."), None, None, w, 6)?;
        w.flush()?;
        Ok((w.hex_digest(), w.count))
    })
    .await??;
    log(server, format!("Archive created ({size} bytes), uploading to destination..."));

    let result = async {
        let file = tokio::fs::File::open(&tmp).await?;
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
        let res = reqwest::Client::builder()
            .timeout(Duration::from_secs(6 * 60 * 60))
            .build()?
            .post(url)
            .header(reqwest::header::AUTHORIZATION, token)
            .header(reqwest::header::CONTENT_TYPE, "application/gzip")
            .header(reqwest::header::CONTENT_LENGTH, size)
            .header("X-Checksum-Sha1", &checksum)
            .body(body)
            .send()
            .await
            .context("uploading archive to destination")?;
        if !res.status().is_success() {
            let status = res.status();
            let text = res.text().await.unwrap_or_default();
            bail!("destination responded {status}: {text}");
        }
        Ok(())
    }
    .await;
    let _ = tokio::fs::remove_file(&tmp).await;
    result
}

async fn cancel_outgoing(ServerRef(s): ServerRef) -> ApiResult<Json<Value>> {
    let handle = OUTGOING.lock().unwrap().remove(s.uuid());
    match handle {
        Some(h) => {
            h.abort();
            s.set_transferring(false);
            log(&s, "Transfer cancelled.");
            s.publish(ServerEvent::TransferStatus("cancelled".into()));
            let _ = tokio::fs::remove_file(
                s.ctx.config.system.tmp_directory.join(format!("{}-transfer.tar.gz", s.uuid())),
            )
            .await;
            Ok(Json(json!({ "status": "ok", "server_id": s.uuid() })))
        }
        None => Err(ApiError::not_found("no transfer in progress for this server")),
    }
}

async fn incoming(State(app): State<AppRef>, headers: HeaderMap, req: Request) -> ApiResult<Json<Value>> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::unauthorized("missing bearer token"))?;
    let claims: TransferClaims = app
        .jwt
        .decode(token)
        .map_err(|_| ApiError::unauthorized("invalid or expired transfer token"))?;
    let uuid = claims.sub;
    if uuid::Uuid::parse_str(&uuid).is_err() {
        return Err(ApiError::bad_request("invalid server uuid in token"));
    }
    if let Some(jti) = &claims.base.jti
        && !app.jwt.consume_once(jti, claims.base.exp)
    {
        return Err(ApiError::forbidden("transfer token already used"));
    }
    if app.manager.get(&uuid).is_some() {
        return Err(ApiError::conflict("server already exists on this node"));
    }
    let expected = headers
        .get("X-Checksum-Sha1")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_ascii_lowercase())
        .ok_or_else(|| ApiError::bad_request("missing X-Checksum-Sha1 header"))?;

    let cfg = app
        .remote
        .get_server_configuration(&uuid)
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("could not fetch server configuration: {e:#}")))?;
    let server = Server::new(app.ctx.clone(), cfg);
    server.set_transferring(true);
    server.fs.ensure_root().await?;
    app.manager.add(server.clone());
    log(&server, "Receiving server archive from source node...");

    let result = receive(&server, req, &expected).await;
    match result {
        Ok(bytes) => {
            server.set_transferring(false);
            server.fs.disk_usage(true).await;
            log(&server, format!("Transfer received and extracted ({bytes} bytes)."));
            server.publish(ServerEvent::TransferStatus("completed".into()));
            if let Err(e) = app.remote.set_transfer_status(&uuid, true).await {
                tracing::warn!(server = %uuid, error = %e, "failed to report transfer success");
            }
            app.manager.save_states();
            Ok(Json(json!({ "status": "ok", "server_id": uuid, "bytes": bytes })))
        }
        Err(e) => {
            tracing::error!(server = %uuid, error = ?e, "incoming transfer failed");
            app.manager.remove(&uuid);
            let _ = server.fs.delete_root().await;
            let _ = app.remote.set_transfer_status(&uuid, false).await;
            Err(ApiError::bad_request(format!("transfer failed: {e}")))
        }
    }
}

async fn receive(server: &Arc<Server>, req: Request, expected: &str) -> Result<u64> {
    let tmp = server
        .ctx
        .config
        .system
        .tmp_directory
        .join(format!("{}-incoming.tar.gz", server.uuid()));
    let result = async {
        let mut file = tokio::fs::File::create(&tmp).await?;
        let mut hasher = <sha1::Sha1 as sha1::Digest>::new();
        let mut stream = req.into_body().into_data_stream().map_err(std::io::Error::other);
        let limit = server.fs.disk_limit();
        let mut total = 0u64;
        while let Some(chunk) = stream.try_next().await? {
            total += chunk.len() as u64;
            if limit > 0 && total > limit {
                bail!("archive exceeds the server's disk limit");
            }
            sha1::Digest::update(&mut hasher, &chunk);
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        let got = hex::encode(sha1::Digest::finalize(hasher));
        if got != expected {
            bail!("checksum mismatch (expected {expected}, got {got})");
        }
        let fs = server.fs.clone();
        let path = tmp.clone();
        let n = tokio::task::spawn_blocking(move || -> Result<u64> {
            let f = std::fs::File::open(&path)?;
            Ok(extract_tar_gz(&fs, std::io::BufReader::new(f), Path::new("."))?)
        })
        .await??;
        Ok(n)
    }
    .await;
    let _ = tokio::fs::remove_file(&tmp).await;
    result
}
