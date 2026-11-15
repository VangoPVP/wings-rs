use crate::context::AppRef;
use crate::error::ApiResult;
use axum::{Json, Router, extract::State, routing::get};
use crate::protocol::wings_api::SystemInfo;

pub fn routes() -> Router<AppRef> {
    Router::new().route("/system", get(system_info))
}


async fn system_info(State(app): State<AppRef>) -> ApiResult<Json<SystemInfo>> {
    let (kernel, os_name, mem) = tokio::task::spawn_blocking(|| {
        let mem = sysinfo::System::new()
            .map(|mut sys| {
                sys.refresh_memory();
                sys.total_memory()
            })
            .unwrap_or(0);
        (
            sysinfo::System::kernel_version().unwrap_or_default(),
            sysinfo::System::long_os_version().unwrap_or_default(),
            mem,
        )
    })
    .await
    .map_err(anyhow::Error::from)?;
    let docker_version = app.docker.version().await.ok().and_then(|v| v.version);
    Ok(Json(SystemInfo {
        version: env!("CARGO_PKG_VERSION").into(),
        architecture: std::env::consts::ARCH.into(),
        os: std::env::consts::OS.into(),
        kernel_version: kernel,
        os_name,
        cpu_count: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        memory_bytes: mem,
        docker_version,
        servers: app.manager.all().len(),
        uptime_seconds: app.started_at.elapsed().as_secs(),
    }))
}
