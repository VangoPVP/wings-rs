#![allow(dead_code)]

use super::schemas::ErrorResponse;
use crate::protocol::{configuration::*, remote::*};
use utoipa::{
    Modify, OpenApi,
    openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme},
};

const DESCRIPTION: &str = r#"Endpoints **the Panel must implement** for wings to call (daemon → Panel).

Base URL: the `remote` value in wings' `config.yml` + `/api/remote`.

## Authentication
Every request carries `Authorization: Bearer <token_id>.<token>` (the node's `token_id` and `token` from `config.yml`) and `Accept: application/vnd.pterodactyl.v1+json`. Reject unknown nodes with 401/403, and only let a node read or modify **its own** servers.

## Responses
Wings treats any 2xx as success and ignores the body unless a response schema is listed. Any non-2xx is an error. 4xx responses are not retried. Endpoints marked **ext** are additions to the Pterodactyl protocol.
"#;

#[derive(OpenApi)]
#[openapi(
    info(title = "Panel remote API (implement this)", description = DESCRIPTION),
    paths(
        list_servers, server_configuration, installation_script, installation_status,
        reset_servers, report_states, report_event, transfer_success, transfer_failure,
        sftp_auth, backup_upload_urls, backup_status, restore_status, activity,
    ),
    components(schemas(ErrorResponse)),
    modifiers(&Security),
    tags(
        (name = "servers", description = "Server configuration and state"),
        (name = "sftp", description = "SFTP login validation"),
        (name = "backups", description = "Backup and restore results"),
        (name = "activity", description = "Audit log"),
    ),
    security(("node_credentials" = [])),
)]
pub struct PanelRemoteApi;

struct Security;

impl Modify for Security {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        openapi.components.get_or_insert_with(Default::default).add_security_scheme(
            "node_credentials",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some("`<token_id>.<token>` of the calling node."))
                    .build(),
            ),
        );
    }
}

#[utoipa::path(
    get, path = "/api/remote/servers", tag = "servers",
    params(
        ("page" = u32, Query, description = "1-based page"),
        ("per_page" = u32, Query, description = "Page size (`remote_query.boot_servers_per_page`)"),
    ),
    responses((status = 200, description = "One page of server configurations", body = Paginated<ServerConfigurationResponse>)),
)]
fn list_servers() {}

#[utoipa::path(
    get, path = "/api/remote/servers/{uuid}", tag = "servers",
    params(("uuid" = String, Path, description = "Server UUID")),
    responses(
        (status = 200, description = "Server configuration", body = ServerConfigurationResponse),
        (status = 404, description = "Unknown server or not on this node", body = ErrorResponse),
    ),
)]
fn server_configuration() {}

#[utoipa::path(
    get, path = "/api/remote/servers/{uuid}/install", tag = "servers",
    params(("uuid" = String, Path, description = "Server UUID")),
    responses((status = 200, description = "Install script", body = InstallationScript)),
)]
fn installation_script() {}

#[utoipa::path(
    post, path = "/api/remote/servers/{uuid}/install", tag = "servers",
    params(("uuid" = String, Path, description = "Server UUID")),
    request_body = InstallStatus,
    responses((status = 204, description = "Recorded")),
)]
fn installation_status() {}

#[utoipa::path(
    post, path = "/api/remote/servers/reset", tag = "servers",
    responses((status = 204, description = "Reset")),
)]
fn reset_servers() {}

#[utoipa::path(
    post, path = "/api/remote/servers/states", tag = "servers",
    request_body = StateReportBatch,
    responses((status = 204, description = "Recorded")),
)]
fn report_states() {}

#[utoipa::path(
    post, path = "/api/remote/servers/{uuid}/events", tag = "servers",
    params(("uuid" = String, Path, description = "Server UUID")),
    request_body = ServerEventReport,
    responses((status = 204, description = "Recorded")),
)]
fn report_event() {}

#[utoipa::path(
    post, path = "/api/remote/servers/{uuid}/transfer/success", tag = "servers",
    params(("uuid" = String, Path, description = "Server UUID")),
    responses((status = 204, description = "Recorded")),
)]
fn transfer_success() {}

#[utoipa::path(
    post, path = "/api/remote/servers/{uuid}/transfer/failure", tag = "servers",
    params(("uuid" = String, Path, description = "Server UUID")),
    responses((status = 204, description = "Recorded")),
)]
fn transfer_failure() {}

#[utoipa::path(
    post, path = "/api/remote/sftp/auth", tag = "sftp",
    request_body = SftpAuthRequest,
    responses(
        (status = 200, description = "Login allowed", body = SftpAuthResponse),
        (status = 403, description = "Login denied", body = ErrorResponse),
    ),
)]
fn sftp_auth() {}

#[utoipa::path(
    get, path = "/api/remote/backups/{uuid}", tag = "backups",
    params(
        ("uuid" = String, Path, description = "Backup UUID"),
        ("size" = u64, Query, description = "Archive size in bytes"),
    ),
    responses((status = 200, description = "Upload URLs", body = BackupUploadUrls)),
)]
fn backup_upload_urls() {}

#[utoipa::path(
    post, path = "/api/remote/backups/{uuid}", tag = "backups",
    params(("uuid" = String, Path, description = "Backup UUID")),
    request_body = BackupStatus,
    responses((status = 204, description = "Recorded")),
)]
fn backup_status() {}

#[utoipa::path(
    post, path = "/api/remote/backups/{uuid}/restore", tag = "backups",
    params(("uuid" = String, Path, description = "Backup UUID")),
    request_body = RestoreStatus,
    responses((status = 204, description = "Recorded")),
)]
fn restore_status() {}

#[utoipa::path(
    post, path = "/api/remote/activity", tag = "activity",
    request_body = ActivityBatch,
    responses((status = 204, description = "Recorded")),
)]
fn activity() {}
