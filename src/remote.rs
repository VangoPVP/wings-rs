use crate::config::Config;
use crate::server::configuration::{InstallationScript, ServerConfigurationResponse};
use anyhow::{Context, Result, bail};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde::de::DeserializeOwned;
pub use crate::protocol::remote::*;
use std::time::Duration;

#[derive(Clone)]
pub struct RemoteClient {
    http: reqwest::Client,
    base: String,
    auth: String,
    per_page: u32,
}

impl RemoteClient {
    pub fn new(config: &Config) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.remote_query.timeout))
            .user_agent(concat!("wings-rs/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            http,
            base: format!("{}/api/remote", config.remote),
            auth: format!("Bearer {}.{}", config.token_id, config.token),
            per_page: config.remote_query.boot_servers_per_page,
        })
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}{}", self.base, path))
            .header("Authorization", &self.auth)
            .header("Accept", "application/vnd.pterodactyl.v1+json")
    }

    async fn send(&self, req: RequestBuilder) -> Result<reqwest::Response> {
        let res = req.send().await.context("panel request failed")?;
        let status = res.status();
        if !status.is_success() {
            let url = res.url().path().to_string();
            let body = res.text().await.unwrap_or_default();
            bail!(RemoteError { status, url, body });
        }
        Ok(res)
    }

    async fn json<T: DeserializeOwned>(&self, req: RequestBuilder) -> Result<T> {
        Ok(self.send(req).await?.json::<T>().await?)
    }

    pub async fn get_servers(&self) -> Result<Vec<ServerConfigurationResponse>> {
        let mut out = Vec::new();
        let mut page = 1;
        loop {
            let res: Paginated<ServerConfigurationResponse> = self
                .json(
                    self.request(Method::GET, "/servers")
                        .query(&[("page", page), ("per_page", self.per_page)]),
                )
                .await?;
            out.extend(res.data);
            if res.meta.current_page >= res.meta.last_page {
                break;
            }
            page += 1;
        }
        Ok(out)
    }

    pub async fn get_server_configuration(&self, uuid: &str) -> Result<ServerConfigurationResponse> {
        self.json(self.request(Method::GET, &format!("/servers/{uuid}")))
            .await
    }

    pub async fn get_installation_script(&self, uuid: &str) -> Result<InstallationScript> {
        self.json(self.request(Method::GET, &format!("/servers/{uuid}/install")))
            .await
    }

    pub async fn set_installation_status(
        &self,
        uuid: &str,
        successful: bool,
        reinstall: bool,
    ) -> Result<()> {
        self.send(
            self.request(Method::POST, &format!("/servers/{uuid}/install"))
                .json(&serde_json::json!({ "successful": successful, "reinstall": reinstall })),
        )
        .await?;
        Ok(())
    }

    pub async fn reset_servers_state(&self) -> Result<()> {
        self.send(self.request(Method::POST, "/servers/reset"))
            .await?;
        Ok(())
    }

    pub async fn validate_sftp_credentials(&self, req: &SftpAuthRequest) -> Result<SftpAuthResponse> {
        self.json(self.request(Method::POST, "/sftp/auth").json(req))
            .await
    }

    pub async fn get_backup_upload_urls(&self, backup: &str, size: u64) -> Result<BackupUploadUrls> {
        self.json(
            self.request(Method::GET, &format!("/backups/{backup}"))
                .query(&[("size", size)]),
        )
        .await
    }

    pub async fn set_backup_status(&self, backup: &str, status: &BackupStatus) -> Result<()> {
        self.send(
            self.request(Method::POST, &format!("/backups/{backup}"))
                .json(status),
        )
        .await?;
        Ok(())
    }

    pub async fn send_restoration_status(&self, backup: &str, successful: bool) -> Result<()> {
        self.send(
            self.request(Method::POST, &format!("/backups/{backup}/restore"))
                .json(&serde_json::json!({ "successful": successful })),
        )
        .await?;
        Ok(())
    }

    pub async fn set_transfer_status(&self, uuid: &str, successful: bool) -> Result<()> {
        let path = if successful { "success" } else { "failure" };
        self.send(self.request(Method::POST, &format!("/servers/{uuid}/transfer/{path}")))
            .await?;
        Ok(())
    }

    pub async fn report_states(&self, servers: &[ServerStateReport]) -> Result<()> {
        self.send(
            self.request(Method::POST, "/servers/states")
                .json(&serde_json::json!({ "data": servers })),
        )
        .await?;
        Ok(())
    }

    pub async fn report_event(&self, uuid: &str, event: &ServerEventReport) -> Result<()> {
        self.send(
            self.request(Method::POST, &format!("/servers/{uuid}/events"))
                .json(event),
        )
        .await?;
        Ok(())
    }

    pub async fn send_activity(&self, entries: &[ActivityEntry]) -> Result<()> {
        self.send(
            self.request(Method::POST, "/activity")
                .json(&serde_json::json!({ "data": entries })),
        )
        .await?;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("panel returned {status} for {url}: {body}")]
pub struct RemoteError {
    pub status: StatusCode,
    pub url: String,
    pub body: String,
}

impl RemoteError {
    pub fn is_client_error(err: &anyhow::Error) -> bool {
        err.downcast_ref::<RemoteError>()
            .is_some_and(|e| e.status.is_client_error())
    }
}
