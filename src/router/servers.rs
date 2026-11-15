use super::ServerRef;
use crate::context::AppRef;
use crate::error::{ApiError, ApiJson, ApiQuery, ApiResult};
use crate::server::Server;
use crate::server::events::PowerAction;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

pub fn routes() -> Router<AppRef> {
    Router::new()
        .route("/servers", get(list).post(create))
        .route("/servers/{server}", get(details))
        .route("/servers/{server}", delete(destroy))
        .route("/servers/{server}/logs", get(logs))
        .route("/servers/{server}/power", post(power))
        .route("/servers/{server}/commands", post(commands))
        .route("/servers/{server}/install", post(install))
        .route("/servers/{server}/reinstall", post(reinstall))
        .route("/servers/{server}/sync", post(sync))
        .route("/servers/{server}/ws/deny", post(ws_deny))
}

pub use crate::protocol::wings_api::ServerDetails;
use crate::protocol::wings_api::{CommandsRequest, CreateServerRequest as CreateRequest, DenyRequest, PowerRequest};

pub fn details_of(s: &Server) -> ServerDetails {
    ServerDetails {
        uuid: s.uuid().to_string(),
        state: s.state(),
        is_suspended: s.is_suspended(),
        is_installing: s.is_installing(),
        is_transferring: s.is_transferring(),
        is_restoring: s.is_restoring(),
        utilization: s.resources(),
    }
}

async fn list(State(app): State<AppRef>) -> Json<Vec<ServerDetails>> {
    Json(app.manager.all().iter().map(|s| details_of(s)).collect())
}

async fn details(ServerRef(s): ServerRef) -> Json<ServerDetails> {
    Json(details_of(&s))
}


async fn create(
    State(app): State<AppRef>,
    ApiJson(req): ApiJson<CreateRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if uuid::Uuid::parse_str(&req.uuid).is_err() {
        return Err(ApiError::bad_request("uuid must be a valid UUID"));
    }
    if app.manager.get(&req.uuid).is_some() {
        return Err(ApiError::conflict("server already exists on this node"));
    }
    let cfg = app
        .remote
        .get_server_configuration(&req.uuid)
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("could not fetch server configuration: {e:#}")))?;
    app.manager.create(cfg, req.start_on_completion).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "status": "accepted", "server_id": req.uuid, "action": "install" })),
    ))
}

async fn destroy(
    State(app): State<AppRef>,
    ServerRef(s): ServerRef,
) -> ApiResult<(StatusCode, Json<Value>)> {
    app.manager.remove(s.uuid());
    let uuid = s.uuid().to_string();
    tokio::spawn(async move {
        if let Err(e) = s.destroy().await {
            tracing::error!(server = %s.uuid(), error = %e, "failed to delete server");
        }
    });
    app.manager.save_states();
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "status": "accepted", "server_id": uuid, "action": "delete" })),
    ))
}

#[derive(Deserialize)]
struct LogsQuery {
    size: Option<usize>,
}

async fn logs(
    ServerRef(s): ServerRef,
    ApiQuery(q): ApiQuery<LogsQuery>,
) -> Json<Value> {
    let size = q.size.unwrap_or(100).clamp(1, 1000);
    Json(json!({ "data": s.console_history(size) }))
}


#[derive(Serialize)]
struct PowerResponse {
    status: &'static str,
    server_id: String,
    action: PowerAction,
}

async fn power(
    ServerRef(s): ServerRef,
    ApiJson(req): ApiJson<PowerRequest>,
) -> ApiResult<(StatusCode, Json<PowerResponse>)> {
    if req.action != PowerAction::Kill {
        s.ensure_idle()?;
    }
    if req.action == PowerAction::Start && s.is_suspended() {
        return Err(ApiError::forbidden("server is suspended"));
    }
    let wait = req.wait_seconds.map(|w| Duration::from_secs(w.min(300)));
    let server = s.clone();
    let action = req.action;
    tokio::spawn(async move {
        if let Err(e) = server.handle_power(action, wait).await {
            tracing::warn!(server = %server.uuid(), ?action, error = %e, "power action failed");
            server.daemon_message(format!("Power action failed: {e}"));
        }
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(PowerResponse {
            status: "accepted",
            server_id: s.uuid().to_string(),
            action: req.action,
        }),
    ))
}


async fn commands(
    State(app): State<AppRef>,
    ServerRef(s): ServerRef,
    ApiJson(req): ApiJson<CommandsRequest>,
) -> ApiResult<Json<Value>> {
    if req.commands.is_empty() {
        return Err(ApiError::bad_request("commands must not be empty"));
    }
    for c in &req.commands {
        s.send_command(c).await?;
        app.activity
            .record(s.uuid(), None, "server:console.command", json!({ "command": c }), None);
    }
    Ok(Json(json!({ "status": "ok", "sent": req.commands.len() })))
}

async fn install(ServerRef(s): ServerRef) -> ApiResult<(StatusCode, Json<Value>)> {
    start_install(s, false)
}

async fn reinstall(ServerRef(s): ServerRef) -> ApiResult<(StatusCode, Json<Value>)> {
    start_install(s, true)
}

fn start_install(s: Arc<Server>, reinstall: bool) -> ApiResult<(StatusCode, Json<Value>)> {
    s.ensure_idle()?;
    let server = s.clone();
    tokio::spawn(async move {
        let _ = server.install(reinstall).await;
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "accepted",
            "server_id": s.uuid(),
            "action": if reinstall { "reinstall" } else { "install" },
        })),
    ))
}

async fn sync(ServerRef(s): ServerRef) -> ApiResult<Json<Value>> {
    s.sync()
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("sync failed: {e:#}")))?;
    if s.is_suspended() && s.is_running() {
        let server = s.clone();
        tokio::spawn(async move {
            server.daemon_message("Server has been suspended, stopping...");
            let _ = server.handle_power(PowerAction::Kill, None).await;
        });
    }
    Ok(Json(json!({ "status": "ok", "server_id": s.uuid() })))
}


async fn ws_deny(
    State(app): State<AppRef>,
    ServerRef(_s): ServerRef,
    ApiJson(req): ApiJson<DenyRequest>,
) -> Json<Value> {
    let n = req.jtis.len();
    app.jwt.deny(req.jtis);
    Json(json!({ "status": "ok", "denied": n }))
}
