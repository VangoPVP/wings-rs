use super::events::{PowerAction, PowerState};
use super::{Server, ServerError};
use crate::environment::ExitInfo;
use crate::remote::{RemoteError, ServerEventReport};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const STOP_TIMEOUT: Duration = Duration::from_secs(600);

impl Server {
    pub async fn handle_power(
        self: &Arc<Self>,
        action: PowerAction,
        lock_wait: Option<Duration>,
    ) -> Result<(), ServerError> {
        if action != PowerAction::Kill {
            self.ensure_idle()?;
        }
        if action == PowerAction::Kill {
            return self.kill().await;
        }

        let _guard = match lock_wait {
            Some(d) if !d.is_zero() => tokio::time::timeout(d, self.power_lock.lock())
                .await
                .map_err(|_| ServerError::PowerLocked)?,
            _ => self
                .power_lock
                .try_lock()
                .map_err(|_| ServerError::PowerLocked)?,
        };

        match action {
            PowerAction::Start => self.start().await,
            PowerAction::Stop => self.stop_and_wait(STOP_TIMEOUT).await,
            PowerAction::Restart => {
                self.stop_and_wait(STOP_TIMEOUT).await?;
                self.start().await
            }
            PowerAction::Kill => unreachable!(),
        }
    }

    async fn start(self: &Arc<Self>) -> Result<(), ServerError> {
        if self.state() != PowerState::Offline {
            return Err(ServerError::AlreadyRunning);
        }
        self.set_state(PowerState::Starting);
        match self.start_inner().await {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::warn!(server = %self.uuid(), error = %e, "failed to start server");
                self.daemon_message(format!("Failed to start server: {e}"));
                self.abort_run_tasks();
                self.set_state(PowerState::Offline);
                Err(e)
            }
        }
    }

    async fn start_inner(self: &Arc<Self>) -> Result<(), ServerError> {
        if let Err(e) = self.sync().await {
            if RemoteError::is_client_error(&e) {
                return Err(e.into());
            }
            tracing::warn!(server = %self.uuid(), error = %e, "config sync failed, using cached config");
        }
        if self.is_suspended() {
            return Err(ServerError::Suspended);
        }
        if !self.fs.has_space_available(false).await {
            return Err(ServerError::DiskSpace);
        }
        self.fs
            .ensure_root()
            .await
            .map_err(|e| ServerError::Other(e.into()))?;

        let cfg = self.config();
        self.daemon_message("Updating process configuration files...");
        crate::configs::apply_all(self, &cfg).await;

        self.daemon_message("Pulling Docker container image, this could take a few minutes...");
        self.env
            .create(&cfg, &self.ctx.config, self.fs.root_path())
            .await?;

        self.expect_exit.store(false, Ordering::SeqCst);
        self.attach().await?;
        self.env.start().await?;
        self.watch_process();

        if cfg.process_configuration.startup.done.is_empty() {
            self.set_state(PowerState::Running);
        }
        Ok(())
    }

    async fn stop_and_wait(self: &Arc<Self>, timeout: Duration) -> Result<(), ServerError> {
        if self.state() == PowerState::Offline {
            if self.env.is_running().await.unwrap_or(false) {
                self.expect_exit.store(true, Ordering::SeqCst);
                self.env.kill("SIGKILL").await?;
            }
            return Ok(());
        }
        self.expect_exit.store(true, Ordering::SeqCst);
        self.set_state(PowerState::Stopping);
        self.send_stop_signal().await?;

        let mut rx = self.subscribe_state();
        let exited = tokio::time::timeout(timeout, rx.wait_for(|s| *s == PowerState::Offline))
            .await
            .is_ok();
        if !exited {
            self.daemon_message("Server did not stop in time, terminating process...");
            self.env.kill("SIGKILL").await?;
            let _ = tokio::time::timeout(
                Duration::from_secs(30),
                rx.wait_for(|s| *s == PowerState::Offline),
            )
            .await;
        }
        Ok(())
    }

    async fn send_stop_signal(self: &Arc<Self>) -> Result<(), ServerError> {
        let stop = self.config().process_configuration.stop;
        match stop.kind.as_str() {
            "command" if !stop.value.is_empty() => {
                if self.write_stdin(&stop.value).await.is_err() {
                    self.env.stop(30).await?;
                }
            }
            "signal" if !stop.value.is_empty() => {
                let sig = match stop.value.as_str() {
                    "^C" => "SIGINT",
                    "^\\" => "SIGQUIT",
                    other => other,
                };
                self.env.kill(sig).await?;
            }
            _ => self.env.stop(30).await?,
        }
        Ok(())
    }

    async fn kill(self: &Arc<Self>) -> Result<(), ServerError> {
        if !self.env.is_running().await.unwrap_or(false) {
            self.set_state(PowerState::Offline);
            return Ok(());
        }
        self.expect_exit.store(true, Ordering::SeqCst);
        self.env.kill("SIGKILL").await?;
        Ok(())
    }

    pub(crate) async fn handle_crash(self: &Arc<Self>, exit: ExitInfo) {
        let global = &self.ctx.config.system.crash_detection;
        let enabled = global.enabled && self.config().settings.crash_detection_enabled;
        if !enabled {
            return;
        }
        if exit.exit_code == 0 && !exit.oom_killed && !global.detect_clean_exit_as_crash {
            return;
        }

        self.daemon_message("---------- Detected server process in a crashed state! ----------");
        self.daemon_message(format!("Exit code: {}", exit.exit_code));
        self.daemon_message(format!("Out of memory: {}", exit.oom_killed));

        let timeout = Duration::from_secs(global.timeout);
        let restart = {
            let mut last = self.last_crash.lock().unwrap();
            let recent = last.is_some_and(|t| t.elapsed() < timeout);
            *last = Some(Instant::now());
            !recent
        };

        let report = ServerEventReport {
            event: "crashed".into(),
            exit_code: Some(exit.exit_code),
            oom_killed: exit.oom_killed,
            restarting: restart,
            timestamp: chrono::Utc::now(),
        };
        let remote = self.ctx.remote.clone();
        let uuid = self.uuid().to_string();
        tokio::spawn(async move {
            if let Err(e) = remote.report_event(&uuid, &report).await {
                tracing::warn!(server = %uuid, error = %e, "failed to report crash to panel");
            }
        });

        if !restart {
            self.daemon_message(format!(
                "Aborting automatic restart, last crash occurred less than {} seconds ago.",
                global.timeout
            ));
            return;
        }
        let server = self.clone();
        tokio::spawn(async move {
            if let Err(e) = server.handle_power(PowerAction::Start, Some(Duration::from_secs(30))).await {
                tracing::warn!(server = %server.uuid(), error = %e, "automatic restart failed");
            }
        });
    }

    pub async fn reattach_if_running(self: &Arc<Self>) -> anyhow::Result<bool> {
        if !self.env.is_running().await? {
            return Ok(false);
        }
        self.attach().await?;
        self.set_state(PowerState::Running);
        self.watch_process();
        Ok(true)
    }
}
