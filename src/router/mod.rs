pub mod backups;
pub mod downloads;
pub mod files;
pub mod servers;
pub mod system;
pub mod transfers;
pub mod ws;

use crate::context::AppRef;
use crate::error::ApiError;
use crate::server::Server;
use axum::{
    Router,
    extract::{FromRequestParts, Path, Request, State},
    http::{HeaderValue, Method, request::Parts},
    middleware::{self, Next},
    response::Response,
};
use std::collections::HashMap;
use std::sync::Arc;
use tower_http::cors::{AllowOrigin, CorsLayer};

pub fn build(app: AppRef) -> Router {
    let panel_api = Router::new()
        .merge(system::routes())
        .merge(servers::routes())
        .merge(transfers::panel_routes())
        .nest("/servers/{server}/files", files::routes())
        .nest("/servers/{server}/backup", backups::routes())
        .route_layer(middleware::from_fn_with_state(app.clone(), require_node_token));

    let public = Router::new()
        .route("/api/servers/{server}/ws", axum::routing::get(ws::handler))
        .merge(downloads::routes())
        .merge(backups::public_routes())
        .merge(transfers::public_routes());

    let mut router = Router::new().nest("/api", panel_api).merge(public);
    if app.config.api.docs {
        router = router.merge(crate::openapi::router());
    }

    router
        .layer(cors(&app))
        .layer(tower_http::catch_panic::CatchPanicLayer::new())
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(app)
}

fn cors(app: &AppRef) -> CorsLayer {
    let mut origins: Vec<HeaderValue> = vec![];
    let allow_all = app.config.allowed_origins.iter().any(|o| o == "*");
    for o in std::iter::once(&app.config.remote).chain(app.config.allowed_origins.iter()) {
        if let Ok(v) = HeaderValue::from_str(o) {
            origins.push(v);
        }
    }
    CorsLayer::new()
        .allow_origin(if allow_all {
            AllowOrigin::any()
        } else {
            AllowOrigin::list(origins)
        })
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::PATCH, Method::DELETE])
        .allow_headers([
            axum::http::header::AUTHORIZATION,
            axum::http::header::CONTENT_TYPE,
            axum::http::header::ACCEPT,
        ])
        .max_age(std::time::Duration::from_secs(7200))
}

pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn require_node_token(
    State(app): State<AppRef>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::unauthorized("missing Authorization header"))?;
    let token = header
        .strip_prefix("Bearer ")
        .ok_or_else(|| ApiError::unauthorized("expected a Bearer token"))?;
    if !ct_eq(token.as_bytes(), app.config.token.as_bytes()) {
        return Err(ApiError::forbidden("invalid token"));
    }
    Ok(next.run(req).await)
}

pub struct ServerRef(pub Arc<Server>);

impl FromRequestParts<AppRef> for ServerRef {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, app: &AppRef) -> Result<Self, Self::Rejection> {
        let Path(params) = Path::<HashMap<String, String>>::from_request_parts(parts, app)
            .await
            .map_err(|_| ApiError::bad_request("invalid path"))?;
        let uuid = params
            .get("server")
            .ok_or_else(|| ApiError::bad_request("missing server id"))?;
        app.manager.get(uuid).map(ServerRef).ok_or_else(|| {
            if app.manager.is_booted() {
                ApiError::not_found("server not found")
            } else {
                ApiError::new(
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "still loading servers from the panel; try again shortly",
                )
            }
        })
    }
}

impl From<crate::server::ServerError> for ApiError {
    fn from(e: crate::server::ServerError) -> Self {
        use crate::server::ServerError as E;
        match e {
            E::Installing | E::Transferring | E::Restoring | E::PowerLocked | E::AlreadyRunning => {
                ApiError::conflict(e.to_string())
            }
            E::Killed => ApiError::conflict(e.to_string()),
            E::NotRunning => ApiError::bad_request(e.to_string()),
            E::Suspended => ApiError::forbidden(e.to_string()),
            E::DiskSpace => ApiError::new(axum::http::StatusCode::INSUFFICIENT_STORAGE, e.to_string()),
            E::Other(err) => err.into(),
        }
    }
}
