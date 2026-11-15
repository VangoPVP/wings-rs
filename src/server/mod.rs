pub mod configuration;
pub mod console;
pub mod events;
pub mod install;
pub mod manager;
pub mod power;

use crate::context::Ctx;
use crate::environment::DockerEnvironment;
use crate::filesystem::Filesystem;
use configuration::ServerConfigurationResponse;
use events::{PowerState, ResourceUsage, ServerEvent};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::io::AsyncWrite;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

pub type StdinWriter = Pin<Box<dyn AsyncWrite + Send>>;

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("server is currently being installed")]
    Installing,
    #[error("server is currently being transferred")]
    Transferring,
    #[error("server is currently restoring a backup")]
    Restoring,
    #[error("server is suspended")]
    Suspended,
    #[error("server is already running")]
    AlreadyRunning,
    #[error("server is not running")]
    NotRunning,
    #[error("another power action is currently being processed for this server, please try again later")]
    PowerLocked,
    #[error("not enough disk space available to start the server")]
    DiskSpace,
    #[error("{0:#}")]
    Other(#[from] anyhow::Error),
}

pub struct Server {
    uuid: String,
    pub ctx: Arc<Ctx>,
    cfg: RwLock<ServerConfigurationResponse>,
    pub fs: Arc<Filesystem>,
    pub env: DockerEnvironment,
    state: watch::Sender<PowerState>,
    events: broadcast::Sender<ServerEvent>,
    console: Mutex<VecDeque<String>>,
    resources: Mutex<ResourceUsage>,
    power_lock: tokio::sync::Mutex<()>,
    stdin: tokio::sync::Mutex<Option<StdinWriter>>,
    expect_exit: AtomicBool,
    installing: AtomicBool,
    transferring: AtomicBool,
    restoring: AtomicBool,
    last_crash: Mutex<Option<Instant>>,
    run_tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl Server {
    pub fn new(ctx: Arc<Ctx>, cfg: ServerConfigurationResponse) -> Arc<Self> {
        let uuid = cfg.settings.uuid.clone();
        let fs = Arc::new(Filesystem::new(
            ctx.config.server_volume(&uuid),
            cfg.settings.build.disk_bytes(),
            ctx.config.system.user.uid,
            ctx.config.system.user.gid,
            &cfg.settings.egg.file_denylist,
            Duration::from_secs(ctx.config.system.disk_check_interval),
        ));
        let env = DockerEnvironment::new(ctx.docker.clone(), &uuid);
        let (state, _) = watch::channel(PowerState::Offline);
        let (events, _) = broadcast::channel(1024);
        let log_count = ctx.config.system.websocket_log_count;
        Arc::new(Self {
            uuid,
            ctx,
            cfg: RwLock::new(cfg),
            fs,
            env,
            state,
            events,
            console: Mutex::new(VecDeque::with_capacity(log_count)),
            resources: Mutex::new(ResourceUsage::default()),
            power_lock: tokio::sync::Mutex::new(()),
            stdin: tokio::sync::Mutex::new(None),
            expect_exit: AtomicBool::new(false),
            installing: AtomicBool::new(false),
            transferring: AtomicBool::new(false),
            restoring: AtomicBool::new(false),
            last_crash: Mutex::new(None),
            run_tasks: Mutex::new(Vec::new()),
        })
    }

    pub fn uuid(&self) -> &str {
        &self.uuid
    }

    pub fn config(&self) -> ServerConfigurationResponse {
        self.cfg.read().unwrap().clone()
    }

    pub fn is_suspended(&self) -> bool {
        self.cfg.read().unwrap().settings.suspended
    }

    pub fn set_config(&self, cfg: ServerConfigurationResponse) {
        self.fs.set_disk_limit(cfg.settings.build.disk_bytes());
        self.fs.set_denylist(&cfg.settings.egg.file_denylist);
        *self.cfg.write().unwrap() = cfg;
    }

    pub async fn sync(&self) -> anyhow::Result<()> {
        let cfg = self.ctx.remote.get_server_configuration(&self.uuid).await?;
        anyhow::ensure!(
            cfg.settings.uuid == self.uuid,
            "panel returned configuration for a different server"
        );
        self.set_config(cfg);
        Ok(())
    }

    pub fn state(&self) -> PowerState {
        *self.state.borrow()
    }

    pub fn subscribe_state(&self) -> watch::Receiver<PowerState> {
        self.state.subscribe()
    }

    pub fn set_state(&self, s: PowerState) {
        let changed = self.state.send_if_modified(|cur| {
            if *cur == s {
                false
            } else {
                *cur = s;
                true
            }
        });
        if changed {
            tracing::debug!(server = %self.uuid, state = s.as_str(), "state change");
            self.resources.lock().unwrap().state = s;
            self.publish(ServerEvent::Status(s));
            self.ctx.states_changed.notify_one();
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state(), PowerState::Running | PowerState::Starting)
    }

    pub fn is_installing(&self) -> bool {
        self.installing.load(Ordering::SeqCst)
    }
    pub fn is_transferring(&self) -> bool {
        self.transferring.load(Ordering::SeqCst)
    }
    pub fn is_restoring(&self) -> bool {
        self.restoring.load(Ordering::SeqCst)
    }
    pub fn set_transferring(&self, v: bool) {
        self.transferring.store(v, Ordering::SeqCst);
    }
    pub fn set_restoring(&self, v: bool) {
        self.restoring.store(v, Ordering::SeqCst);
    }
    pub(crate) fn begin_installing(&self) -> bool {
        !self.installing.swap(true, Ordering::SeqCst)
    }
    pub(crate) fn end_installing(&self) {
        self.installing.store(false, Ordering::SeqCst);
    }

    pub fn ensure_idle(&self) -> Result<(), ServerError> {
        if self.is_installing() {
            return Err(ServerError::Installing);
        }
        if self.is_transferring() {
            return Err(ServerError::Transferring);
        }
        if self.is_restoring() {
            return Err(ServerError::Restoring);
        }
        Ok(())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.events.subscribe()
    }

    pub fn publish(&self, ev: ServerEvent) {
        let _ = self.events.send(ev);
    }

    pub fn daemon_message(&self, msg: impl Into<String>) {
        self.publish(ServerEvent::DaemonMessage(msg.into()));
    }

    pub fn push_console_line(&self, line: String) {
        {
            let max = self.ctx.config.system.websocket_log_count.max(1);
            let mut c = self.console.lock().unwrap();
            while c.len() >= max {
                c.pop_front();
            }
            c.push_back(line.clone());
        }
        self.publish(ServerEvent::ConsoleOutput(line));
    }

    pub fn console_history(&self, lines: usize) -> Vec<String> {
        let c = self.console.lock().unwrap();
        let skip = c.len().saturating_sub(lines);
        c.iter().skip(skip).cloned().collect()
    }

    pub async fn send_command(&self, cmd: &str) -> Result<(), ServerError> {
        if !self.is_running() {
            return Err(ServerError::NotRunning);
        }
        self.write_stdin(cmd).await
    }

    pub(crate) async fn write_stdin(&self, cmd: &str) -> Result<(), ServerError> {
        use tokio::io::AsyncWriteExt;
        let mut guard = self.stdin.lock().await;
        let Some(w) = guard.as_mut() else {
            return Err(ServerError::NotRunning);
        };
        let line = format!("{}\n", cmd.trim_end_matches(['\r', '\n']));
        w.write_all(line.as_bytes())
            .await
            .map_err(|e| ServerError::Other(e.into()))?;
        w.flush().await.map_err(|e| ServerError::Other(e.into()))?;
        Ok(())
    }

    pub fn resources(&self) -> ResourceUsage {
        let mut r = self.resources.lock().unwrap().clone();
        r.disk_bytes = self.fs.cached_usage();
        r.state = self.state();
        r
    }

    pub(crate) fn update_resources(&self, f: impl FnOnce(&mut ResourceUsage)) -> ResourceUsage {
        let mut r = self.resources.lock().unwrap();
        f(&mut r);
        r.disk_bytes = self.fs.cached_usage();
        r.clone()
    }

    pub(crate) fn add_run_task(&self, h: JoinHandle<()>) {
        let mut t = self.run_tasks.lock().unwrap();
        t.retain(|h| !h.is_finished());
        t.push(h);
    }

    pub(crate) fn abort_run_tasks(&self) {
        for h in self.run_tasks.lock().unwrap().drain(..) {
            h.abort();
        }
    }

    pub async fn destroy(&self) -> anyhow::Result<()> {
        self.expect_exit.store(true, Ordering::SeqCst);
        self.abort_run_tasks();
        let _ = self.env.kill("SIGKILL").await;
        self.env.remove().await?;
        self.fs.delete_root().await?;
        self.set_state(PowerState::Offline);
        self.publish(ServerEvent::Deleted);
        Ok(())
    }
}
