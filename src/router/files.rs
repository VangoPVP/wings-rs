use super::ServerRef;
use crate::context::AppRef;
use crate::error::{ApiError, ApiJson, ApiQuery, ApiResult};
use crate::filesystem::archive::ArchiveFormat;
use crate::filesystem::pull::{PullOptions, pull};
use crate::filesystem::{FileStat, clean_path, join_clean};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use futures_util::TryStreamExt;
use serde::Deserialize;
use crate::protocol::files::{
    ChmodRequest, ChmodResponse, CompressRequest, CopyRequest, CreateDirectoryRequest as CreateDirRequest,
    DecompressRequest, DecompressResponse, DeleteResponse, FilesRequest, PullRequest, RenameRequest, RenameResponse,
};
use serde_json::{Value, json};
use std::path::PathBuf;

pub fn routes() -> Router<AppRef> {
    Router::new()
        .route("/list-directory", get(list_directory))
        .route("/contents", get(contents))
        .route("/write", post(write).layer(DefaultBodyLimit::disable()))
        .route("/create-directory", post(create_directory))
        .route("/rename", put(rename))
        .route("/copy", post(copy))
        .route("/delete", post(delete))
        .route("/compress", post(compress))
        .route("/decompress", post(decompress))
        .route("/chmod", post(chmod))
        .route("/pull", post(pull_file))
}

fn record(app: &AppRef, server: &str, event: &str, meta: Value) {
    app.activity.record(server, None, &format!("server:file.{event}"), meta, None);
}

#[derive(Deserialize)]
struct DirQuery {
    #[serde(default)]
    directory: String,
}

async fn list_directory(
    ServerRef(s): ServerRef,
    ApiQuery(q): ApiQuery<DirQuery>,
) -> ApiResult<Json<Vec<FileStat>>> {
    let path = clean_path(&q.directory)?;
    Ok(Json(s.fs.list_directory(path).await?))
}

#[derive(Deserialize)]
struct FileQuery {
    file: String,
    #[serde(default)]
    download: Option<String>,
}

pub(crate) async fn stream_file(
    fs: &std::sync::Arc<crate::filesystem::Filesystem>,
    path: PathBuf,
    attachment: bool,
) -> ApiResult<Response> {
    let (file, len) = fs.open_async_read(path.clone()).await?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let mime = mime_guess::from_path(&name).first_or_octet_stream();
    let body = Body::from_stream(tokio_util::io::ReaderStream::new(file));
    let mut res = body.into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime.essence_str())
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    h.insert("X-Content-Type-Options", HeaderValue::from_static("nosniff"));
    if attachment {
        let safe: String = name
            .chars()
            .map(|c| if c == '"' || c == '\\' || c.is_control() { '_' } else { c })
            .collect();
        if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{safe}\"")) {
            h.insert(header::CONTENT_DISPOSITION, v);
        }
    }
    Ok(res)
}

async fn contents(ServerRef(s): ServerRef, ApiQuery(q): ApiQuery<FileQuery>) -> ApiResult<Response> {
    let path = clean_path(&q.file)?;
    s.fs.check_denied(&path)?;
    let attach = matches!(q.download.as_deref(), Some("1" | "true"));
    stream_file(&s.fs, path, attach).await
}

#[derive(Deserialize)]
struct WriteQuery {
    file: String,
}

async fn write(
    State(app): State<AppRef>,
    ServerRef(s): ServerRef,
    ApiQuery(q): ApiQuery<WriteQuery>,
    req: Request,
) -> ApiResult<Json<FileStat>> {
    let path = clean_path(&q.file)?;
    s.fs.check_denied(&path)?;
    let stream = req.into_body().into_data_stream().map_err(std::io::Error::other);
    let reader = tokio_util::io::StreamReader::new(stream);
    s.fs.write_stream(path.clone(), reader, None).await?;
    record(&app, s.uuid(), "write", json!({ "file": q.file }));
    Ok(Json(s.fs.stat(path).await?))
}


async fn create_directory(
    State(app): State<AppRef>,
    ServerRef(s): ServerRef,
    ApiJson(req): ApiJson<CreateDirRequest>,
) -> ApiResult<Json<FileStat>> {
    let path = join_clean(&req.path, &req.name)?;
    s.fs.create_directory(path.clone()).await?;
    record(&app, s.uuid(), "create-directory", json!({ "directory": req.path, "name": req.name }));
    Ok(Json(s.fs.stat(path).await?))
}



async fn rename(
    State(app): State<AppRef>,
    ServerRef(s): ServerRef,
    ApiJson(req): ApiJson<RenameRequest>,
) -> ApiResult<Json<RenameResponse>> {
    if req.files.is_empty() {
        return Err(ApiError::bad_request("files must not be empty"));
    }
    let mut n = 0;
    for f in &req.files {
        let from = join_clean(&req.root, &f.from)?;
        let to = join_clean(&req.root, &f.to)?;
        s.fs.rename(from, to).await?;
        n += 1;
    }
    let files: Vec<_> = req.files.iter().map(|f| json!({ "from": f.from, "to": f.to })).collect();
    record(&app, s.uuid(), "rename", json!({ "directory": req.root, "files": files }));
    Ok(Json(RenameResponse { renamed: n }))
}


async fn copy(
    State(app): State<AppRef>,
    ServerRef(s): ServerRef,
    ApiJson(req): ApiJson<CopyRequest>,
) -> ApiResult<Json<FileStat>> {
    let stat = s.fs.copy(clean_path(&req.location)?).await?;
    record(&app, s.uuid(), "copy", json!({ "file": req.location }));
    Ok(Json(stat))
}


async fn delete(
    State(app): State<AppRef>,
    ServerRef(s): ServerRef,
    ApiJson(req): ApiJson<FilesRequest>,
) -> ApiResult<Json<DeleteResponse>> {
    if req.files.is_empty() {
        return Err(ApiError::bad_request("files must not be empty"));
    }
    let paths = req
        .files
        .iter()
        .map(|f| join_clean(&req.root, f))
        .collect::<Result<Vec<_>, _>>()?;
    let n = s.fs.delete(paths).await?;
    record(&app, s.uuid(), "delete", json!({ "directory": req.root, "files": req.files }));
    Ok(Json(DeleteResponse { deleted: n }))
}


async fn compress(
    State(app): State<AppRef>,
    ServerRef(s): ServerRef,
    ApiJson(req): ApiJson<CompressRequest>,
) -> ApiResult<Json<FileStat>> {
    if req.files.is_empty() {
        return Err(ApiError::bad_request("files must not be empty"));
    }
    let format = match req.format.as_deref() {
        None => ArchiveFormat::TarGz,
        Some(f) => ArchiveFormat::parse(f)
            .ok_or_else(|| ApiError::bad_request("format must be tar.gz, tar or zip"))?,
    };
    let root = clean_path(&req.root)?;
    let files = req
        .files
        .iter()
        .map(|f| clean_path(f))
        .collect::<Result<Vec<_>, _>>()?;
    let stat = s.fs.compress(root, files, format).await?;
    record(&app, s.uuid(), "compress", json!({ "directory": req.root, "files": req.files }));
    Ok(Json(stat))
}


async fn decompress(
    State(app): State<AppRef>,
    ServerRef(s): ServerRef,
    ApiJson(req): ApiJson<DecompressRequest>,
) -> ApiResult<Json<DecompressResponse>> {
    let root = clean_path(&req.root)?;
    let file = join_clean(&req.root, &req.file)?;
    let bytes = s.fs.decompress(root, file).await?;
    record(&app, s.uuid(), "decompress", json!({ "directory": req.root, "file": req.file }));
    Ok(Json(DecompressResponse { status: "ok".into(), bytes_written: bytes }))
}



async fn chmod(
    State(app): State<AppRef>,
    ServerRef(s): ServerRef,
    ApiJson(req): ApiJson<ChmodRequest>,
) -> ApiResult<Json<ChmodResponse>> {
    for f in &req.files {
        let mode = u32::from_str_radix(&f.mode, 8)
            .ok()
            .filter(|m| *m <= 0o777)
            .ok_or_else(|| ApiError::bad_request(format!("invalid mode '{}'", f.mode)))?;
        s.fs.chmod(join_clean(&req.root, &f.file)?, mode).await?;
    }
    let files: Vec<_> = req.files.iter().map(|f| json!({ "file": f.file, "mode": f.mode })).collect();
    record(&app, s.uuid(), "chmod", json!({ "directory": req.root, "files": files }));
    Ok(Json(ChmodResponse { updated: req.files.len() }))
}


async fn pull_file(
    State(app): State<AppRef>,
    ServerRef(s): ServerRef,
    ApiJson(req): ApiJson<PullRequest>,
) -> ApiResult<Response> {
    if !s.fs.has_space_available(true).await {
        return Err(ApiError::new(StatusCode::INSUFFICIENT_STORAGE, "not enough disk space available"));
    }
    let opts = PullOptions {
        url: req.url.clone(),
        directory: clean_path(&req.directory)?,
        filename: req.filename.clone(),
        use_header: req.use_header,
    };
    record(&app, s.uuid(), "pull", json!({ "url": req.url, "directory": req.directory }));
    if req.foreground {
        let stat = pull(&s.fs, opts).await?;
        return Ok(Json(stat).into_response());
    }
    let server = s.clone();
    tokio::spawn(async move {
        match pull(&server.fs, opts).await {
            Ok(stat) => server.daemon_message(format!("Finished downloading {}", stat.name)),
            Err(e) => {
                tracing::warn!(server = %server.uuid(), error = %e, "pull failed");
                server.daemon_message(format!("File download failed: {e}"));
            }
        }
    });
    Ok((StatusCode::ACCEPTED, Json(json!({ "status": "accepted" }))).into_response())
}
