pub mod remote;
pub mod schemas;
pub mod wings;

use axum::Router;
use utoipa::OpenApi;
use utoipa_swagger_ui::{SwaggerUi, Url};

pub const DOCS_PATH: &str = "/docs";
pub const WINGS_SPEC_PATH: &str = "/docs/wings.json";
pub const REMOTE_SPEC_PATH: &str = "/docs/panel-remote.json";

pub fn wings_spec() -> utoipa::openapi::OpenApi {
    let mut spec = wings::WingsApi::openapi();
    spec.info.version = env!("CARGO_PKG_VERSION").into();
    spec
}

pub fn remote_spec() -> utoipa::openapi::OpenApi {
    let mut spec = remote::PanelRemoteApi::openapi();
    spec.info.version = env!("CARGO_PKG_VERSION").into();
    spec
}

pub fn router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    SwaggerUi::new(DOCS_PATH)
        .urls(vec![
            (Url::new("wings API (Panel -> daemon)", WINGS_SPEC_PATH), wings_spec()),
            (Url::new("Panel remote API (daemon -> Panel)", REMOTE_SPEC_PATH), remote_spec()),
        ])
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn operations(spec: &utoipa::openapi::OpenApi) -> BTreeSet<String> {
        let json = serde_json::to_value(spec).unwrap();
        let mut out = BTreeSet::new();
        for (path, item) in json["paths"].as_object().unwrap() {
            for method in item.as_object().unwrap().keys() {
                if matches!(method.as_str(), "get" | "post" | "put" | "patch" | "delete") {
                    out.insert(format!("{} {path}", method.to_uppercase()));
                }
            }
        }
        out
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn wings_spec_documents_every_route() {
        let expected = set(&[
            "GET /api/system",
            "GET /api/servers",
            "POST /api/servers",
            "GET /api/servers/{server}",
            "DELETE /api/servers/{server}",
            "GET /api/servers/{server}/logs",
            "POST /api/servers/{server}/power",
            "POST /api/servers/{server}/commands",
            "POST /api/servers/{server}/install",
            "POST /api/servers/{server}/reinstall",
            "POST /api/servers/{server}/sync",
            "POST /api/servers/{server}/ws/deny",
            "GET /api/servers/{server}/files/list-directory",
            "GET /api/servers/{server}/files/contents",
            "POST /api/servers/{server}/files/write",
            "POST /api/servers/{server}/files/create-directory",
            "PUT /api/servers/{server}/files/rename",
            "POST /api/servers/{server}/files/copy",
            "POST /api/servers/{server}/files/delete",
            "POST /api/servers/{server}/files/compress",
            "POST /api/servers/{server}/files/decompress",
            "POST /api/servers/{server}/files/chmod",
            "POST /api/servers/{server}/files/pull",
            "POST /api/servers/{server}/backup",
            "POST /api/servers/{server}/backup/{backup}/restore",
            "DELETE /api/servers/{server}/backup/{backup}",
            "POST /api/servers/{server}/transfer",
            "DELETE /api/servers/{server}/transfer",
            "POST /api/transfers",
            "GET /api/servers/{server}/ws",
            "GET /download/file",
            "GET /download/backup",
            "POST /upload/file",
            "PUT /upload/chunk",
        ]);
        assert_eq!(operations(&wings_spec()), expected);
    }

    #[test]
    fn remote_spec_documents_every_client_call() {
        let expected = set(&[
            "GET /api/remote/servers",
            "GET /api/remote/servers/{uuid}",
            "GET /api/remote/servers/{uuid}/install",
            "POST /api/remote/servers/{uuid}/install",
            "POST /api/remote/servers/reset",
            "POST /api/remote/servers/states",
            "POST /api/remote/servers/{uuid}/events",
            "POST /api/remote/servers/{uuid}/transfer/success",
            "POST /api/remote/servers/{uuid}/transfer/failure",
            "POST /api/remote/sftp/auth",
            "GET /api/remote/backups/{uuid}",
            "POST /api/remote/backups/{uuid}",
            "POST /api/remote/backups/{uuid}/restore",
            "POST /api/remote/activity",
        ]);
        assert_eq!(operations(&remote_spec()), expected);
    }

    #[test]
    fn all_schema_refs_resolve() {
        for spec in [wings_spec(), remote_spec()] {
            let json = serde_json::to_value(&spec).unwrap();
            let schemas = json["components"]["schemas"].as_object().cloned().unwrap_or_default();
            let text = json.to_string();
            for part in text.split("\"#/components/schemas/").skip(1) {
                let name = &part[..part.find('"').unwrap()];
                assert!(schemas.contains_key(name), "{}: unresolved $ref {name}", spec.info.title);
            }
        }
    }
}
