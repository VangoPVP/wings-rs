use super::Server;
use super::configuration::ServerConfigurationResponse;
use super::events::{PowerAction, PowerState};
use crate::context::Ctx;
use crate::remote::{RemoteError, ServerStateReport};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

pub struct Manager {
    ctx: Arc<Ctx>,
    servers: RwLock<HashMap<String, Arc<Server>>>,
    booted: AtomicBool,
}

impl Manager {
    pub fn new(ctx: Arc<Ctx>) -> Self {
        Self {
            ctx,
            servers: RwLock::new(HashMap::new()),
            booted: AtomicBool::new(false),
        }
    }

    pub fn is_booted(&self) -> bool {
        self.booted.load(Ordering::Acquire)
    }

    pub fn get(&self, uuid: &str) -> Option<Arc<Server>> {
        self.servers.read().unwrap().get(uuid).cloned()
    }

    pub fn find_by_short_uuid(&self, short: &str) -> Option<Arc<Server>> {
        self.servers
            .read()
            .unwrap()
            .values()
            .find(|s| s.uuid().split('-').next() == Some(short) || s.uuid().starts_with(short))
            .cloned()
    }

    pub fn all(&self) -> Vec<Arc<Server>> {
        self.servers.read().unwrap().values().cloned().collect()
    }

    pub fn add(&self, server: Arc<Server>) {
        self.servers
            .write()
            .unwrap()
            .insert(server.uuid().to_string(), server);
    }

    pub fn remove(&self, uuid: &str) -> Option<Arc<Server>> {
        self.servers.write().unwrap().remove(uuid)
    }

    pub async fn boot(&self) -> Result<()> {
        let configs = self.ctx.remote.get_servers().await?;
        let previous = self.load_states();
        tracing::info!(count = configs.len(), "loaded servers from panel");
        for cfg in configs {
            if self.get(&cfg.settings.uuid).is_some() {
                continue;
            }
            let server = Server::new(self.ctx.clone(), cfg);
            self.add(server.clone());
            let was = previous.get(server.uuid()).copied();
            tokio::spawn(async move {
                if let Err(e) = server.fs.ensure_root().await {
                    tracing::error!(server = %server.uuid(), error = %e, "failed to create server directory");
                }
                server.fs.disk_usage(true).await;
                match server.reattach_if_running().await {
                    Ok(true) => {
                        tracing::info!(server = %server.uuid(), "re-attached to running server");
                    }
                    Ok(false)
                        if matches!(was, Some(PowerState::Running | PowerState::Starting)) =>
                    {
                        tracing::info!(server = %server.uuid(), "restoring previous running state");
                        if let Err(e) = server.handle_power(PowerAction::Start, None).await {
                            tracing::warn!(server = %server.uuid(), error = %e, "failed to restore state");
                        }
                    }
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!(server = %server.uuid(), error = %e, "failed to inspect container");
                    }
                }
            });
        }
        self.booted.store(true, Ordering::Release);
        Ok(())
    }

    pub async fn boot_with_retry(&self) {
        retry("reset server states on panel", true, || self.ctx.remote.reset_servers_state()).await;
        retry("load servers from panel", false, || self.boot()).await;
    }

    pub async fn create(&self, cfg: ServerConfigurationResponse, start_on_completion: bool) -> Result<Arc<Server>> {
        let server = Server::new(self.ctx.clone(), cfg);
        server.fs.ensure_root().await?;
        self.add(server.clone());
        let s = server.clone();
        // Capture the generation so a kill during/after the install still prevents
        // the start-on-completion from resurrecting the server.
        let generation = s.generation();
        tokio::spawn(async move {
            let ok = s.install(false).await.is_ok();
            if ok && start_on_completion
                && let Err(e) = s
                    .handle_power_from(
                        PowerAction::Start,
                        Some(Duration::from_secs(30)),
                        generation,
                    )
                    .await
            {
                match e {
                    super::ServerError::Killed => {
                        tracing::debug!(server = %s.uuid(), "start after install cancelled by kill")
                    }
                    e => {
                        tracing::warn!(server = %s.uuid(), error = %e, "failed to start after install")
                    }
                }
            }
        });
        Ok(server)
    }

    fn load_states(&self) -> HashMap<String, PowerState> {
        std::fs::read(self.ctx.config.states_file())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save_states(&self) {
        if !self.is_booted() {
            return;
        }
        let states: HashMap<String, PowerState> = self
            .all()
            .iter()
            .map(|s| (s.uuid().to_string(), s.state()))
            .collect();
        let path = self.ctx.config.states_file();
        let tmp = path.with_extension("json.tmp");
        let res = serde_json::to_vec(&states)
            .map_err(std::io::Error::other)
            .and_then(|b| std::fs::write(&tmp, b))
            .and_then(|_| std::fs::rename(&tmp, &path));
        if let Err(e) = res {
            tracing::warn!(error = %e, "failed to persist server states");
        }
    }

    pub async fn run_state_persister(&self) {
        loop {
            self.ctx.states_changed.notified().await;
            tokio::time::sleep(Duration::from_secs(1)).await;
            self.save_states();
        }
    }

    pub fn state_reports(&self) -> Vec<ServerStateReport> {
        self.all()
            .iter()
            .map(|s| ServerStateReport {
                uuid: s.uuid().to_string(),
                state: s.state().as_str().to_string(),
                is_installing: s.is_installing(),
                is_transferring: s.is_transferring(),
                is_restoring: s.is_restoring(),
                utilization: serde_json::to_value(s.resources()).unwrap_or_default(),
            })
            .collect()
    }

    pub async fn run_state_reporter(&self, interval: Duration) {
        if interval.is_zero() {
            return;
        }
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            if !self.is_booted() {
                continue;
            }
            let reports = self.state_reports();
            if reports.is_empty() {
                continue;
            }
            if let Err(e) = self.ctx.remote.report_states(&reports).await {
                tracing::debug!(error = %e, "failed to report server states");
            }
        }
    }
}

async fn retry<F, Fut>(what: &str, give_up_on_client_error: bool, mut op: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let mut delay = Duration::from_secs(1);
    let mut attempt = 1u32;
    loop {
        match op().await {
            Ok(()) => {
                if attempt > 1 {
                    tracing::info!(attempts = attempt, "{what}: succeeded");
                }
                return;
            }
            Err(e) if give_up_on_client_error && RemoteError::is_client_error(&e) => {
                tracing::warn!(error = %format!("{e:#}"), "{what}: failed, not retrying");
                return;
            }
            Err(e) => {
                let error = format!("{e:#}");
                if attempt == 1 || delay == MAX_RETRY_DELAY {
                    tracing::warn!(%error, retry_in = ?delay, "{what}: failed, retrying");
                } else {
                    tracing::debug!(%error, retry_in = ?delay, "{what}: failed, retrying");
                }
            }
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(MAX_RETRY_DELAY);
        attempt += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use tokio::time::Instant;

    fn panel_error(status: u16) -> anyhow::Error {
        RemoteError {
            status: reqwest::StatusCode::from_u16(status).unwrap(),
            url: "/api/remote/servers/reset".into(),
            body: String::new(),
        }
        .into()
    }

    #[tokio::test(start_paused = true)]
    async fn retries_with_capped_backoff_until_success() {
        let calls = AtomicU32::new(0);
        let start = Instant::now();
        retry("test", false, || async {
            if calls.fetch_add(1, Ordering::SeqCst) < 8 {
                anyhow::bail!("connection refused")
            }
            Ok(())
        })
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 9);
        assert_eq!(start.elapsed(), Duration::from_secs(183));
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_on_panel_4xx_only_when_asked() {
        let calls = AtomicU32::new(0);
        retry("test", true, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(panel_error(404))
        })
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let calls = AtomicU32::new(0);
        retry("test", true, || async {
            match calls.fetch_add(1, Ordering::SeqCst) {
                0 => Err(panel_error(502)),
                _ => Ok(()),
            }
        })
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let calls = AtomicU32::new(0);
        retry("test", false, || async {
            match calls.fetch_add(1, Ordering::SeqCst) {
                0 => Err(panel_error(401)),
                _ => Ok(()),
            }
        })
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
