use super::events::{PowerAction, PowerState};
use super::{Server, ServerError};
use crate::environment::{DockerEnvironment, ExitInfo};
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
        let generation = self.generation();
        self.handle_power_from(action, lock_wait, generation).await
    }

    /// Runs a power action decided at `generation`. Crash recovery passes the token of the
    /// run that crashed so a kill landing in between still wins.
    pub(crate) async fn handle_power_from(
        self: &Arc<Self>,
        action: PowerAction,
        lock_wait: Option<Duration>,
        generation: u64,
    ) -> Result<(), ServerError> {
        if action == PowerAction::Kill {
            return self.kill().await;
        }
        self.ensure_idle()?;

        let _guard = match lock_wait {
            Some(d) if !d.is_zero() => tokio::time::timeout(d, self.power_lock.lock())
                .await
                .map_err(|_| ServerError::PowerLocked)?,
            _ => self
                .power_lock
                .try_lock()
                .map_err(|_| ServerError::PowerLocked)?,
        };

        if self.superseded(generation) {
            return Err(ServerError::Killed);
        }

        match action {
            PowerAction::Start => self.start().await,
            PowerAction::Stop => self.stop_and_wait(STOP_TIMEOUT, generation).await,
            PowerAction::Restart => {
                self.stop_and_wait(STOP_TIMEOUT, generation).await?;
                if self.superseded(generation) {
                    return Err(ServerError::Killed);
                }
                self.start().await
            }
            PowerAction::Kill => unreachable!(),
        }
    }

    async fn start(self: &Arc<Self>) -> Result<(), ServerError> {
        if self.state() != PowerState::Offline {
            return Err(ServerError::AlreadyRunning);
        }
        // Own this run: invalidates watchers and in-flight work from the previous run.
        let run = self.bump_generation();
        self.set_state(PowerState::Starting);
        match self.start_inner(run).await {
            Ok(()) => Ok(()),
            Err(ServerError::Killed) => {
                self.abort_run_tasks();
                self.set_state(PowerState::Offline);
                Err(ServerError::Killed)
            }
            Err(e) => {
                tracing::warn!(server = %self.uuid(), error = %e, "failed to start server");
                self.daemon_message(format!("Failed to start server: {e}"));
                self.abort_run_tasks();
                self.set_state(PowerState::Offline);
                Err(e)
            }
        }
    }

    /// Fails with `Killed` once a kill (or newer run) superseded `run`. With
    /// `remove_container`, a container this operation just created is removed too.
    async fn ensure_current(
        self: &Arc<Self>,
        run: u64,
        remove_container: bool,
    ) -> Result<(), ServerError> {
        if !self.superseded(run) {
            return Ok(());
        }
        if remove_container {
            let _ = self.env.remove().await;
        }
        Err(ServerError::Killed)
    }

    async fn start_inner(self: &Arc<Self>, run: u64) -> Result<(), ServerError> {
        if let Err(e) = self.sync().await {
            if RemoteError::is_client_error(&e) {
                return Err(e.into());
            }
            tracing::warn!(server = %self.uuid(), error = %e, "config sync failed, using cached config");
        }
        self.ensure_current(run, false).await?;
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
        self.ensure_current(run, false).await?;

        let cfg = self.config();
        self.daemon_message("Updating process configuration files...");
        crate::configs::apply_all(self, &cfg).await;
        self.ensure_current(run, false).await?;

        self.daemon_message("Pulling Docker container image, this could take a few minutes...");
        if let Err(e) = self
            .env
            .create(&cfg, &self.ctx.config, self.fs.root_path())
            .await
        {
            if self.superseded(run) {
                let _ = self.env.remove().await;
                return Err(ServerError::Killed);
            }
            return Err(e.into());
        }
        // A container exists now; if we lost the race, take it with us.
        self.ensure_current(run, true).await?;

        self.expect_exit.store(false, Ordering::SeqCst);
        if let Err(e) = self.attach().await {
            if self.superseded(run) {
                let _ = self.env.remove().await;
                return Err(ServerError::Killed);
            }
            return Err(e.into());
        }
        self.ensure_current(run, true).await?;
        if let Err(e) = self.env.start().await {
            if self.superseded(run) {
                let _ = self.env.remove().await;
                return Err(ServerError::Killed);
            }
            return Err(e.into());
        }
        self.ensure_current(run, true).await?;
        self.watch_process(run);

        if cfg.process_configuration.startup.done.is_empty() {
            self.set_state(PowerState::Running);
        }
        Ok(())
    }

    /// Graceful stop decided at `run`. A kill that lands mid-stop owns the outcome: we
    /// must not leave the state stuck on `stopping` after the container is gone.
    async fn stop_and_wait(
        self: &Arc<Self>,
        timeout: Duration,
        run: u64,
    ) -> Result<(), ServerError> {
        if self.superseded(run) {
            return Err(ServerError::Killed);
        }
        if self.state() == PowerState::Offline {
            if self.env.is_running().await.unwrap_or(false) {
                if self.superseded(run) {
                    return Err(ServerError::Killed);
                }
                self.expect_exit.store(true, Ordering::SeqCst);
                self.env.kill("SIGKILL").await?;
            }
            return Ok(());
        }
        self.expect_exit.store(true, Ordering::SeqCst);
        self.set_state(PowerState::Stopping);
        if self.superseded(run) {
            self.set_state(PowerState::Offline);
            return Err(ServerError::Killed);
        }
        if let Err(e) = self.send_stop_signal().await {
            if self.superseded(run) {
                self.set_state(PowerState::Offline);
                return Err(ServerError::Killed);
            }
            return Err(e);
        }

        let mut rx = self.subscribe_state();
        let exited = tokio::time::timeout(timeout, rx.wait_for(|s| *s == PowerState::Offline))
            .await
            .is_ok();
        if !exited {
            if self.superseded(run) {
                self.set_state(PowerState::Offline);
                return Err(ServerError::Killed);
            }
            self.daemon_message("Server did not stop in time, terminating process...");
            if let Err(e) = self.env.kill("SIGKILL").await {
                if self.superseded(run) {
                    self.set_state(PowerState::Offline);
                    return Err(ServerError::Killed);
                }
                return Err(e.into());
            }
            let _ = tokio::time::timeout(
                Duration::from_secs(30),
                rx.wait_for(|s| *s == PowerState::Offline),
            )
            .await;
        }
        if self.superseded(run) {
            self.set_state(PowerState::Offline);
            return Err(ServerError::Killed);
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

    /// Hard kill: cancels anything in flight and force-removes the server's containers.
    /// Unlike stop, this leaves nothing behind regardless of whether the process was
    /// running, starting, installing or already finished.
    async fn kill(self: &Arc<Self>) -> Result<(), ServerError> {
        // Invalidate in-flight start/restart/install and stale process watchers first.
        self.bump_generation();
        self.expect_exit.store(true, Ordering::SeqCst);
        self.abort_run_tasks();
        // The install task may still be unwinding (e.g. a chown pass); don't make the
        // server look busy while it does.
        self.end_installing();
        *self.stdin.lock().await = None;
        self.update_resources(|r| {
            r.memory_bytes = 0;
            r.cpu_absolute = 0.0;
            r.uptime = 0;
        });

        // Helper containers must go too, otherwise a running install keeps writing files.
        for name in [
            format!("{}_installer", self.uuid()),
            format!("{}_chown", self.uuid()),
        ] {
            let helper = DockerEnvironment::new(self.env.docker().clone(), &name);
            if let Err(e) = helper.remove().await {
                tracing::warn!(
                    server = %self.uuid(),
                    container = %name,
                    error = %e,
                    "failed to remove helper container during kill"
                );
            }
        }

        // force=true kills a running container with SIGKILL and removes it either way.
        // Errors other than "not found" are real failures, so surface them instead of
        // pretending the server is offline.
        self.env
            .remove()
            .await
            .map_err(|e| ServerError::Other(e.context("removing server container")))?;
        self.set_state(PowerState::Offline);
        Ok(())
    }

    pub(crate) async fn handle_crash(self: &Arc<Self>, run: u64, exit: ExitInfo) {
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
            match server
                .handle_power_from(
                    PowerAction::Start,
                    Some(Duration::from_secs(30)),
                    run,
                )
                .await
            {
                Ok(()) => {}
                Err(ServerError::Killed) => {
                    tracing::debug!(server = %server.uuid(), "crash restart cancelled by kill")
                }
                Err(e) => {
                    tracing::warn!(server = %server.uuid(), error = %e, "automatic restart failed")
                }
            }
        });
    }

    pub async fn reattach_if_running(self: &Arc<Self>) -> anyhow::Result<bool> {
        if !self.env.is_running().await? {
            return Ok(false);
        }
        let run = self.bump_generation();
        self.attach().await?;
        self.set_state(PowerState::Running);
        self.watch_process(run);
        Ok(true)
    }
}
