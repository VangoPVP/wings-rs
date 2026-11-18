#![allow(dead_code)]

use super::schemas::*;
use crate::protocol::{
    files::*,
    server::{NetworkUsage, PowerAction, PowerState, ResourceUsage},
    wings_api::*,
};
use utoipa::{
    Modify, OpenApi,
    openapi::security::{ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityScheme},
};

const DESCRIPTION: &str = r#"HTTP API of the wings daemon.

## Authentication
* **`/api/*`** (Panel → daemon): `Authorization: Bearer <node token>` (the `token` in `config.yml`). Wrong or missing token → 401/403.
* **Browser endpoints** (`/download/*`, `/upload/*`): `?token=<jwt>`. The Panel mints an HS256 JWT signed with the **node token**.
* **Websocket** (`/api/servers/{server}/ws`): no HTTP auth. Send `{"event":"auth","args":["<jwt>"]}` as the first frame (see the websocket operation).
* **`POST /api/transfers`** (node → node): `Authorization: Bearer <jwt>` signed with the **destination** node's token.

## JWT claims
Common: `exp` (required), `nbf`, `iat`, `jti`. Omit unused claims (don't send `null`).

| Token | Extra claims |
|---|---|
| websocket | `user_uuid`, `server_uuid`, `permissions: [..]` |
| file download | `server_uuid`, `file_path`, `unique_id` (single use) |
| backup download | `server_uuid`, `backup_uuid`, `unique_id` (single use) |
| upload | `server_uuid`, `user_uuid`, `unique_id` (reusable until `exp`) |
| transfer | `sub` = server uuid, `jti` (single use) |

## Errors
Every error body is `{"error": "<message>"}`. File operations return 403 for denylisted paths, 409 if the target exists, 413 if too large and 507 when the disk quota is exhausted.

Right after the daemon starts, until it has loaded its servers from the Panel (retried with backoff while the Panel is unreachable), routes under `/api/servers/{server}` return **503** for servers it doesn't know yet instead of 404.
"#;

#[derive(OpenApi)]
#[openapi(
    info(title = "wings", description = DESCRIPTION),
    paths(
        system_info,
        list_servers, create_server, server_details, delete_server, server_logs, server_power,
        server_commands, server_install, server_reinstall, server_sync, server_ws_deny,
        list_directory, file_contents, write_file, create_directory, rename_files, copy_file,
        delete_files, compress_files, decompress_file, chmod_files, pull_file,
        create_backup, restore_backup, delete_backup,
        start_transfer, cancel_transfer, receive_transfer,
        websocket, download_file, download_backup, upload_file, upload_chunk,
    ),
    components(schemas(
        ErrorResponse, WsMessage, ResourceUsage, NetworkUsage, PowerState, PowerAction,
    )),
    modifiers(&Security),
    tags(
        (name = "system", description = "Node information"),
        (name = "servers", description = "Server lifecycle and console"),
        (name = "files", description = "File management. Paths are relative to the server root; `..` and symlinks can't escape it."),
        (name = "backups", description = "Backups (`.pteroignore` in the server root is merged into `ignore`)"),
        (name = "transfers", description = "Moving a server between nodes.\n\n1. Assign the server to the destination node in the Panel.\n2. Call the **source** node's `POST /api/servers/{server}/transfer`.\n3. The source stops the server and pushes a tar.gz to the destination's `POST /api/transfers`.\n4. The destination reports `/api/remote/servers/{uuid}/transfer/success` or `/failure`; the source reports `/failure` if the push fails.\n5. On success, `DELETE /api/servers/{server}` on the source."),
        (name = "browser", description = "Endpoints the browser talks to directly, authenticated with Panel-minted JWTs"),
    ),
    security(("node_token" = [])),
)]
pub struct WingsApi;

struct Security;

impl Modify for Security {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let c = openapi.components.get_or_insert_with(Default::default);
        c.add_security_scheme(
            "node_token",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some("The node's `token` from config.yml."))
                    .build(),
            ),
        );
        c.add_security_scheme(
            "jwt_query",
            SecurityScheme::ApiKey(ApiKey::Query(ApiKeyValue::with_description(
                "token",
                "HS256 JWT minted by the Panel, signed with the node token.",
            ))),
        );
        c.add_security_scheme(
            "transfer_jwt",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("JWT")
                    .description(Some("HS256 JWT signed with the destination node's token, `sub` = server uuid."))
                    .build(),
            ),
        );
    }
}


#[utoipa::path(
    get, path = "/api/system", tag = "system",
    responses((status = 200, description = "Host information", body = SystemInfo)),
)]
fn system_info() {}


#[utoipa::path(
    get, path = "/api/servers", tag = "servers",
    responses((status = 200, description = "Every server on this node", body = Vec<ServerDetails>)),
)]
fn list_servers() {}

#[utoipa::path(
    post, path = "/api/servers", tag = "servers",
    request_body = CreateServerRequest,
    responses(
        (status = 202, description = "Install started", body = AcceptedResponse),
        (status = 400, description = "Invalid UUID", body = ErrorResponse),
        (status = 409, description = "Server already exists on this node", body = ErrorResponse),
        (status = 502, description = "Could not fetch the configuration from the Panel", body = ErrorResponse),
    ),
)]
fn create_server() {}

#[utoipa::path(
    get, path = "/api/servers/{server}", tag = "servers",
    params(("server" = String, Path, description = "Server UUID")),
    responses(
        (status = 200, description = "Server state", body = ServerDetails),
        (status = 404, description = "Server not found", body = ErrorResponse),
    ),
)]
fn server_details() {}

#[utoipa::path(
    delete, path = "/api/servers/{server}", tag = "servers",
    params(("server" = String, Path, description = "Server UUID")),
    responses(
        (status = 202, description = "Deletion started", body = AcceptedResponse),
        (status = 404, description = "Server not found", body = ErrorResponse),
    ),
)]
fn delete_server() {}

#[utoipa::path(
    get, path = "/api/servers/{server}/logs", tag = "servers",
    params(
        ("server" = String, Path, description = "Server UUID"),
        ("size" = Option<usize>, Query, description = "Number of lines (1-1000, default 100)"),
    ),
    responses(
        (status = 200, description = "Recent console output", body = LogsResponse),
        (status = 404, description = "Server not found", body = ErrorResponse),
    ),
)]
fn server_logs() {}

#[utoipa::path(
    post, path = "/api/servers/{server}/power", tag = "servers",
    params(("server" = String, Path, description = "Server UUID")),
    request_body = PowerRequest,
    responses(
        (status = 200, description = "Container killed and removed (kill)", body = PowerResponse),
        (status = 202, description = "Action queued (start/stop/restart)", body = PowerResponse),
        (status = 403, description = "Server is suspended", body = ErrorResponse),
        (status = 404, description = "Server not found", body = ErrorResponse),
        (status = 409, description = "Server is installing, transferring, restoring or another power action is running", body = ErrorResponse),
        (status = 500, description = "Docker failed to remove the container during a kill", body = ErrorResponse),
        (status = 504, description = "Kill timed out", body = ErrorResponse),
    ),
)]
fn server_power() {}

#[utoipa::path(
    post, path = "/api/servers/{server}/commands", tag = "servers",
    params(("server" = String, Path, description = "Server UUID")),
    request_body = CommandsRequest,
    responses(
        (status = 200, description = "Commands sent", body = CommandsResponse),
        (status = 400, description = "Empty list or server not running", body = ErrorResponse),
        (status = 404, description = "Server not found", body = ErrorResponse),
        (status = 409, description = "Server is busy", body = ErrorResponse),
    ),
)]
fn server_commands() {}

#[utoipa::path(
    post, path = "/api/servers/{server}/install", tag = "servers",
    params(("server" = String, Path, description = "Server UUID")),
    responses(
        (status = 202, description = "Install started", body = AcceptedResponse),
        (status = 404, description = "Server not found", body = ErrorResponse),
        (status = 409, description = "Server is busy", body = ErrorResponse),
    ),
)]
fn server_install() {}

#[utoipa::path(
    post, path = "/api/servers/{server}/reinstall", tag = "servers",
    params(("server" = String, Path, description = "Server UUID")),
    responses(
        (status = 202, description = "Reinstall started", body = AcceptedResponse),
        (status = 404, description = "Server not found", body = ErrorResponse),
        (status = 409, description = "Server is busy", body = ErrorResponse),
    ),
)]
fn server_reinstall() {}

#[utoipa::path(
    post, path = "/api/servers/{server}/sync", tag = "servers",
    params(("server" = String, Path, description = "Server UUID")),
    responses(
        (status = 200, description = "Configuration updated", body = ServerOkResponse),
        (status = 404, description = "Server not found", body = ErrorResponse),
        (status = 502, description = "Could not fetch the configuration from the Panel", body = ErrorResponse),
    ),
)]
fn server_sync() {}

#[utoipa::path(
    post, path = "/api/servers/{server}/ws/deny", tag = "servers",
    params(("server" = String, Path, description = "Server UUID")),
    request_body = DenyRequest,
    responses(
        (status = 200, description = "Tokens revoked", body = DenyResponse),
        (status = 404, description = "Server not found", body = ErrorResponse),
    ),
)]
fn server_ws_deny() {}



#[utoipa::path(
    get, path = "/api/servers/{server}/files/list-directory", tag = "files",
    params(
        ("server" = String, Path, description = "Server UUID"),
        ("directory" = Option<String>, Query, description = "Directory to list (default: root)"),
    ),
    responses(
        (status = 200, description = "Entries, directories first", body = Vec<FileStat>),
        (status = 404, description = "Server or directory not found", body = ErrorResponse),
    ),
)]
fn list_directory() {}

#[utoipa::path(
    get, path = "/api/servers/{server}/files/contents", tag = "files",
    params(
        ("server" = String, Path, description = "Server UUID"),
        ("file" = String, Query, description = "File path"),
        ("download" = Option<String>, Query, description = "`1` or `true`: send as an attachment"),
    ),
    responses(
        (status = 200, description = "Raw file contents with a guessed Content-Type", body = BinaryFile, content_type = "application/octet-stream"),
        (status = 403, description = "Path is denylisted", body = ErrorResponse),
        (status = 404, description = "Server or file not found", body = ErrorResponse),
    ),
)]
fn file_contents() {}

#[utoipa::path(
    post, path = "/api/servers/{server}/files/write", tag = "files",
    params(
        ("server" = String, Path, description = "Server UUID"),
        ("file" = String, Query, description = "File path"),
    ),
    request_body(content = BinaryFile, content_type = "application/octet-stream"),
    responses(
        (status = 200, description = "Written file", body = FileStat),
        (status = 403, description = "Path is denylisted", body = ErrorResponse),
        (status = 507, description = "Disk quota exceeded", body = ErrorResponse),
    ),
)]
fn write_file() {}

#[utoipa::path(
    post, path = "/api/servers/{server}/files/create-directory", tag = "files",
    params(("server" = String, Path, description = "Server UUID")),
    request_body = CreateDirectoryRequest,
    responses(
        (status = 200, description = "Created directory", body = FileStat),
        (status = 409, description = "Already exists", body = ErrorResponse),
    ),
)]
fn create_directory() {}

#[utoipa::path(
    put, path = "/api/servers/{server}/files/rename", tag = "files",
    params(("server" = String, Path, description = "Server UUID")),
    request_body = RenameRequest,
    responses(
        (status = 200, description = "Number of renamed entries", body = RenameResponse),
        (status = 400, description = "Empty list", body = ErrorResponse),
        (status = 409, description = "Target exists", body = ErrorResponse),
    ),
)]
fn rename_files() {}

#[utoipa::path(
    post, path = "/api/servers/{server}/files/copy", tag = "files",
    params(("server" = String, Path, description = "Server UUID")),
    request_body = CopyRequest,
    responses(
        (status = 200, description = "The new copy", body = FileStat),
        (status = 507, description = "Disk quota exceeded", body = ErrorResponse),
    ),
)]
fn copy_file() {}

#[utoipa::path(
    post, path = "/api/servers/{server}/files/delete", tag = "files",
    params(("server" = String, Path, description = "Server UUID")),
    request_body = FilesRequest,
    responses(
        (status = 200, description = "Number of deleted entries", body = DeleteResponse),
        (status = 400, description = "Empty list", body = ErrorResponse),
    ),
)]
fn delete_files() {}

#[utoipa::path(
    post, path = "/api/servers/{server}/files/compress", tag = "files",
    params(("server" = String, Path, description = "Server UUID")),
    request_body = CompressRequest,
    responses(
        (status = 200, description = "The created archive", body = FileStat),
        (status = 400, description = "Empty list or unknown format", body = ErrorResponse),
        (status = 507, description = "Disk quota exceeded", body = ErrorResponse),
    ),
)]
fn compress_files() {}

#[utoipa::path(
    post, path = "/api/servers/{server}/files/decompress", tag = "files",
    params(("server" = String, Path, description = "Server UUID")),
    request_body = DecompressRequest,
    responses(
        (status = 200, description = "Extracted", body = DecompressResponse),
        (status = 507, description = "Disk quota exceeded", body = ErrorResponse),
    ),
)]
fn decompress_file() {}

#[utoipa::path(
    post, path = "/api/servers/{server}/files/chmod", tag = "files",
    params(("server" = String, Path, description = "Server UUID")),
    request_body = ChmodRequest,
    responses(
        (status = 200, description = "Number of updated entries", body = ChmodResponse),
        (status = 400, description = "Invalid mode", body = ErrorResponse),
    ),
)]
fn chmod_files() {}

#[utoipa::path(
    post, path = "/api/servers/{server}/files/pull", tag = "files",
    params(("server" = String, Path, description = "Server UUID")),
    request_body = PullRequest,
    responses(
        (status = 200, description = "Downloaded file (`foreground: true`)", body = FileStat),
        (status = 202, description = "Download started", body = StatusResponse),
        (status = 400, description = "URL not allowed", body = ErrorResponse),
        (status = 507, description = "Disk quota exceeded", body = ErrorResponse),
    ),
)]
fn pull_file() {}

#[utoipa::path(
    post, path = "/api/servers/{server}/backup", tag = "backups",
    params(("server" = String, Path, description = "Server UUID")),
    request_body = BackupRequest,
    responses(
        (status = 202, description = "Backup started", body = BackupAcceptedResponse),
        (status = 400, description = "Invalid backup UUID", body = ErrorResponse),
        (status = 409, description = "Server is restoring or transferring", body = ErrorResponse),
    ),
)]
fn create_backup() {}

#[utoipa::path(
    post, path = "/api/servers/{server}/backup/{backup}/restore", tag = "backups",
    params(
        ("server" = String, Path, description = "Server UUID"),
        ("backup" = String, Path, description = "Backup UUID"),
    ),
    request_body = RestoreRequest,
    responses(
        (status = 202, description = "Restore started", body = BackupAcceptedResponse),
        (status = 400, description = "Invalid backup UUID or missing download_url", body = ErrorResponse),
        (status = 404, description = "Backup not found on this node", body = ErrorResponse),
        (status = 409, description = "Server is busy or not offline", body = ErrorResponse),
    ),
)]
fn restore_backup() {}

#[utoipa::path(
    delete, path = "/api/servers/{server}/backup/{backup}", tag = "backups",
    params(
        ("server" = String, Path, description = "Server UUID"),
        ("backup" = String, Path, description = "Backup UUID"),
    ),
    responses(
        (status = 200, description = "Deleted", body = BackupDeletedResponse),
        (status = 400, description = "Invalid backup UUID", body = ErrorResponse),
        (status = 404, description = "Backup not found", body = ErrorResponse),
    ),
)]
fn delete_backup() {}

#[utoipa::path(
    post, path = "/api/servers/{server}/transfer", tag = "transfers",
    params(("server" = String, Path, description = "Server UUID")),
    request_body = TransferRequest,
    responses(
        (status = 202, description = "Transfer started", body = AcceptedResponse),
        (status = 400, description = "Invalid URL", body = ErrorResponse),
        (status = 409, description = "Server is busy", body = ErrorResponse),
    ),
)]
fn start_transfer() {}

#[utoipa::path(
    delete, path = "/api/servers/{server}/transfer", tag = "transfers",
    params(("server" = String, Path, description = "Server UUID")),
    responses(
        (status = 200, description = "Transfer cancelled", body = ServerOkResponse),
        (status = 404, description = "No transfer in progress", body = ErrorResponse),
    ),
)]
fn cancel_transfer() {}

#[utoipa::path(
    post, path = "/api/transfers", tag = "transfers",
    params(("X-Checksum-Sha1" = String, Header, description = "Hex SHA-1 of the body")),
    request_body(content = BinaryFile, content_type = "application/gzip"),
    security(("transfer_jwt" = [])),
    responses(
        (status = 200, description = "Server received and extracted", body = TransferReceivedResponse),
        (status = 400, description = "Missing checksum header or invalid token subject", body = ErrorResponse),
        (status = 401, description = "Missing or invalid token", body = ErrorResponse),
        (status = 403, description = "Token already used", body = ErrorResponse),
        (status = 409, description = "Server already exists on this node", body = ErrorResponse),
    ),
)]
fn receive_transfer() {}

#[utoipa::path(
    get, path = "/api/servers/{server}/ws", tag = "browser",
    params(("server" = String, Path, description = "Server UUID")),
    security(()),
    responses(
        (status = 101, description = "Switched to the websocket protocol"),
        (status = 403, description = "Origin not allowed", body = ErrorResponse),
        (status = 404, description = "Server not found", body = ErrorResponse),
    ),
)]
fn websocket() {}

#[utoipa::path(
    get, path = "/download/file", tag = "browser",
    security(("jwt_query" = [])),
    responses(
        (status = 200, description = "File as an attachment", body = BinaryFile, content_type = "application/octet-stream"),
        (status = 401, description = "Invalid or expired token", body = ErrorResponse),
        (status = 403, description = "Link already used or path denylisted", body = ErrorResponse),
        (status = 404, description = "Server or file not found", body = ErrorResponse),
    ),
)]
fn download_file() {}

#[utoipa::path(
    get, path = "/download/backup", tag = "browser",
    security(("jwt_query" = [])),
    responses(
        (status = 200, description = "Backup archive (`<uuid>.tar.gz`)", body = BinaryFile, content_type = "application/gzip"),
        (status = 401, description = "Invalid or expired token", body = ErrorResponse),
        (status = 403, description = "Link already used", body = ErrorResponse),
        (status = 404, description = "Server or backup not found", body = ErrorResponse),
    ),
)]
fn download_backup() {}

#[utoipa::path(
    post, path = "/upload/file", tag = "browser",
    params(("directory" = Option<String>, Query, description = "Target directory (default: root)")),
    security(("jwt_query" = [])),
    request_body(content = UploadForm, content_type = "multipart/form-data"),
    responses(
        (status = 200, description = "Uploaded files", body = Vec<FileStat>),
        (status = 400, description = "No `files` fields or invalid file name", body = ErrorResponse),
        (status = 401, description = "Invalid or expired token", body = ErrorResponse),
        (status = 413, description = "File exceeds the upload limit", body = ErrorResponse),
        (status = 507, description = "Disk quota exceeded", body = ErrorResponse),
    ),
)]
fn upload_file() {}

#[utoipa::path(
    put, path = "/upload/chunk", tag = "browser",
    params(
        ("file" = String, Query, description = "Target file path"),
        ("offset" = u64, Query, description = "Byte offset of this chunk"),
        ("total" = u64, Query, description = "Final file size in bytes"),
    ),
    security(("jwt_query" = [])),
    request_body(content = BinaryFile, content_type = "application/octet-stream"),
    responses(
        (status = 200, description = "Progress (`ChunkProgressResponse`) until the last chunk, then the finished `FileStat`", body = ChunkProgressResponse),
        (status = 400, description = "Chunk extends past `total` or invalid file name", body = ErrorResponse),
        (status = 401, description = "Invalid or expired token", body = ErrorResponse),
        (status = 409, description = "Wrong offset; resume from `expected_offset`", body = ChunkOffsetConflict),
        (status = 413, description = "Chunk larger than 16 MiB"),
        (status = 507, description = "Disk quota exceeded", body = ErrorResponse),
    ),
)]
fn upload_chunk() {}
