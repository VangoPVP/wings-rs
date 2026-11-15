use super::files::stream_file;
use crate::context::AppRef;
use crate::error::{ApiError, ApiQuery, ApiResult};
use crate::filesystem::{FileStat, FsError, blocking, clean_path, join_clean};
use crate::jwt::{FileDownloadClaims, UploadClaims};
use crate::server::Server;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use futures_util::TryStreamExt;
use serde::Deserialize;
use serde_json::json;
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncReadExt;

const MAX_CHUNK: usize = 16 * 1024 * 1024;

pub fn routes() -> Router<AppRef> {
    Router::new()
        .route("/download/file", get(download_file))
        .route("/upload/file", post(upload_file).layer(DefaultBodyLimit::disable()))
        .route("/upload/chunk", put(upload_chunk).layer(DefaultBodyLimit::max(MAX_CHUNK)))
}

fn server_for(app: &AppRef, uuid: &str) -> ApiResult<Arc<Server>> {
    app.manager
        .get(uuid)
        .ok_or_else(|| ApiError::not_found("server not found"))
}

fn jwt_err(_: crate::jwt::JwtError) -> ApiError {
    ApiError::unauthorized("invalid or expired token")
}

#[derive(Deserialize)]
struct TokenQuery {
    token: String,
}

async fn download_file(State(app): State<AppRef>, ApiQuery(q): ApiQuery<TokenQuery>) -> ApiResult<Response> {
    let claims: FileDownloadClaims = app.jwt.decode(&q.token).map_err(jwt_err)?;
    if !app.jwt.consume_once(&claims.unique_id, claims.base.exp) {
        return Err(ApiError::forbidden("this download link has already been used"));
    }
    let s = server_for(&app, &claims.server_uuid)?;
    let path = clean_path(&claims.file_path)?;
    s.fs.check_denied(&path)?;
    stream_file(&s.fs, path, true).await
}

fn upload_name(raw: &str) -> ApiResult<String> {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if base.is_empty() || base == "." || base == ".." || base.contains('\0') {
        return Err(ApiError::bad_request("invalid file name"));
    }
    Ok(base.to_string())
}

#[derive(Deserialize)]
struct UploadQuery {
    token: String,
    #[serde(default)]
    directory: String,
}

async fn upload_file(
    State(app): State<AppRef>,
    ApiQuery(q): ApiQuery<UploadQuery>,
    mut multipart: Multipart,
) -> ApiResult<Json<Vec<FileStat>>> {
    let claims: UploadClaims = app.jwt.decode(&q.token).map_err(jwt_err)?;
    let s = server_for(&app, &claims.server_uuid)?;
    let limit = app.config.upload_limit_bytes();
    let mut out = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::bad_request(format!("invalid multipart body: {e}")))?
    {
        if field.name() != Some("files") {
            continue;
        }
        let name = upload_name(field.file_name().unwrap_or(""))?;
        let path = join_clean(&q.directory, &name)?;
        s.fs.check_denied(&path)?;
        let stream = field.map_err(std::io::Error::other);
        let reader = tokio_util::io::StreamReader::new(stream);
        s.fs.write_stream(path.clone(), reader, Some(limit))
            .await
            .map_err(|e| match e {
                FsError::TooLarge => ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!("file exceeds the upload limit of {} MiB", app.config.api.upload_limit),
                ),
                other => other.into(),
            })?;
        app.activity.record(
            s.uuid(),
            Some(&claims.user_uuid),
            "server:file.uploaded",
            json!({ "directory": q.directory, "file": name }),
            None,
        );
        out.push(s.fs.stat(path).await?);
    }
    if out.is_empty() {
        return Err(ApiError::bad_request("no files in request (expected multipart field 'files')"));
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
struct ChunkQuery {
    token: String,
    file: String,
    offset: u64,
    total: u64,
}

fn temp_name(path: &Path) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!(".{name}.wings-upload"))
}

async fn upload_chunk(
    State(app): State<AppRef>,
    ApiQuery(q): ApiQuery<ChunkQuery>,
    req: Request,
) -> ApiResult<Response> {
    let claims: UploadClaims = app.jwt.decode(&q.token).map_err(jwt_err)?;
    let s = server_for(&app, &claims.server_uuid)?;
    let path = clean_path(&q.file)?;
    if path == Path::new(".") {
        return Err(ApiError::bad_request("invalid file name"));
    }
    s.fs.check_denied(&path)?;
    let temp = temp_name(&path);

    let mut body = Vec::new();
    req.into_body()
        .into_data_stream()
        .map_err(std::io::Error::other)
        .into_async_read_compat()
        .read_to_end(&mut body)
        .await
        .map_err(|_| ApiError::bad_request("could not read chunk body"))?;
    let end = q.offset + body.len() as u64;
    if end > q.total {
        return Err(ApiError::bad_request("chunk extends past declared total"));
    }

    let current = {
        let fs = s.fs.clone();
        let t = temp.clone();
        blocking(move || match fs.open_root()?.symlink_metadata(&t) {
            Ok(m) if m.is_file() => Ok(m.len()),
            Ok(_) => Err(FsError::InvalidPath("upload temp path is not a file".into())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e.into()),
        })
        .await?
    };
    if q.offset != current {
        return Ok((
            StatusCode::CONFLICT,
            Json(json!({ "error": "unexpected chunk offset", "expected_offset": current })),
        )
            .into_response());
    }
    if q.offset == 0 {
        s.fs.ensure_space_for(q.total).await?;
    } else {
        s.fs.ensure_space_for(body.len() as u64).await?;
    }

    let fs = s.fs.clone();
    let t = temp.clone();
    let len = body.len() as u64;
    blocking(move || {
        use std::io::Write;
        let mut f = fs.open_write(&t, q.offset == 0)?;
        f.seek(SeekFrom::Start(q.offset))?;
        f.write_all(&body)?;
        Ok(())
    })
    .await?;
    s.fs.add_usage(len as i64);

    if end < q.total {
        return Ok(Json(json!({ "received": end, "total": q.total })).into_response());
    }
    let fs = s.fs.clone();
    let (t, p) = (temp.clone(), path.clone());
    blocking(move || {
        let root = fs.open_root()?;
        if let Ok(m) = root.symlink_metadata(&p) {
            if m.is_dir() {
                return Err(FsError::IsDirectory);
            }
            root.remove_file(&p)?;
        }
        root.rename(&t, &root, &p)?;
        Ok(())
    })
    .await?;
    app.activity.record(
        s.uuid(),
        Some(&claims.user_uuid),
        "server:file.uploaded",
        json!({ "file": q.file, "chunked": true }),
        None,
    );
    Ok(Json(s.fs.stat(path).await?).into_response())
}

trait IntoAsyncReadCompat {
    fn into_async_read_compat(self) -> tokio_util::io::StreamReader<Self, bytes::Bytes>
    where
        Self: Sized;
}

impl<S> IntoAsyncReadCompat for S
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>>,
{
    fn into_async_read_compat(self) -> tokio_util::io::StreamReader<Self, bytes::Bytes> {
        tokio_util::io::StreamReader::new(self)
    }
}
