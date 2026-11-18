use super::events::{PowerAction, ServerEvent};
use super::{Server, ServerError};
use crate::environment::{DockerEnvironment, InstallerSpec, environment_vars};
use crate::server::console::LineSplitter;
use bollard::container::LogOutput;
use futures_util::StreamExt;
use std::io::Write;
use std::sync::Arc;

impl Server {
    pub async fn install(self: &Arc<Self>, reinstall: bool) -> Result<(), ServerError> {
        let run = self.generation();
        self.install_from(reinstall, run).await
    }

    /// Installs against a run token captured when the request arrived, so a kill that
    /// lands before this task is even polled still wins.
    pub async fn install_from(
        self: &Arc<Self>,
        reinstall: bool,
        run: u64,
    ) -> Result<(), ServerError> {
        let result = self.install_run(run).await;

        // The panel marks the server "installing" before calling us, so it needs a
        // terminal status even when the attempt was cancelled before it started.
        // `Installing` means another attempt owns the slot and will report itself.
        if !matches!(result, Err(ServerError::Installing)) {
            let successful = result.is_ok();
            if let Err(e) = self
                .ctx
                .remote
                .set_installation_status(self.uuid(), successful, reinstall)
                .await
            {
                tracing::warn!(server = %self.uuid(), error = %e, "failed to report install status");
            }
        }
        result
    }

    async fn install_run(self: &Arc<Self>, run: u64) -> Result<(), ServerError> {
        if self.is_running() {
            self.handle_power_from(
                PowerAction::Stop,
                Some(std::time::Duration::from_secs(30)),
                run,
            )
            .await?;
        }
        if self.superseded(run) {
            return Err(ServerError::Killed);
        }
        if !self.begin_installing() {
            return Err(ServerError::Installing);
        }
        self.publish(ServerEvent::InstallStarted);
        let result = self.install_inner(run).await;
        // A kill already cleared the flag so the server is usable again immediately;
        // skip it here so an attempt started after the kill keeps its slot.
        if !self.superseded(run) {
            self.end_installing();
        }

        match &result {
            Ok(()) => {}
            Err(ServerError::Killed) => {
                tracing::debug!(server = %self.uuid(), "installation cancelled by kill");
                self.publish(ServerEvent::InstallOutput(
                    "Installation cancelled by kill.".into(),
                ));
            }
            Err(e) => {
                tracing::error!(server = %self.uuid(), error = %e, "installation failed");
                self.publish(ServerEvent::InstallOutput(format!("Installation failed: {e}")));
            }
        }
        self.publish(ServerEvent::InstallCompleted { successful: result.is_ok() });
        result
    }

    async fn install_inner(self: &Arc<Self>, run: u64) -> Result<(), ServerError> {
        if let Err(e) = self.sync().await {
            tracing::warn!(server = %self.uuid(), error = %e, "config sync before install failed");
        }
        if self.superseded(run) {
            return Err(ServerError::Killed);
        }
        self.fs
            .ensure_root()
            .await
            .map_err(|e| ServerError::Other(e.into()))?;
        let cfg = self.config();
        if cfg.settings.skip_egg_scripts {
            self.publish(ServerEvent::InstallOutput(
                "Skipping egg installation script (skip_egg_scripts is set).".into(),
            ));
            return Ok(());
        }

        let script = self.ctx.remote.get_installation_script(self.uuid()).await?;
        if self.superseded(run) {
            return Err(ServerError::Killed);
        }
        let config = &self.ctx.config;

        let script_dir = config.system.tmp_directory.join(self.uuid());
        std::fs::create_dir_all(&script_dir).map_err(anyhow::Error::from)?;
        std::fs::write(
            script_dir.join("install.sh"),
            script.script.replace("\r\n", "\n"),
        )
        .map_err(anyhow::Error::from)?;

        let name = format!("{}_installer", self.uuid());
        let limits = &config.docker.installer_limits;
        let memory = cfg.settings.build.memory_limit.max(limits.memory);
        let cpu = if cfg.settings.build.cpu_limit == 0 {
            0
        } else {
            cfg.settings.build.cpu_limit.max(limits.cpu)
        };
        let spec = InstallerSpec {
            name: name.clone(),
            image: &script.container_image,
            entrypoint: &script.entrypoint,
            env: environment_vars(&cfg, config),
            server_volume: self.fs.root_path(),
            script_dir: &script_dir,
            memory_mib: memory,
            cpu,
        };
        self.publish(ServerEvent::InstallOutput(format!(
            "Pulling installation container image {}...",
            script.container_image
        )));
        let installer = DockerEnvironment::new(self.env.docker().clone(), &name);
        if let Err(e) = DockerEnvironment::create_installer(self.env.docker(), &spec, config).await {
            if self.superseded(run) {
                let _ = installer.remove().await;
                return Err(ServerError::Killed);
            }
            return Err(e.into());
        }
        if self.superseded(run) {
            // Killed while we were creating it: take the installer down immediately.
            let _ = installer.remove().await;
            return Err(ServerError::Killed);
        }

        let log_path = config
            .system
            .root_directory
            .join("install_logs")
            .join(format!("{}.log", self.uuid()));
        let _ = std::fs::create_dir_all(log_path.parent().unwrap());
        let mut log = std::fs::File::create(&log_path).ok();

        let attach = match installer.attach().await {
            Ok(a) => a,
            Err(e) => {
                if self.superseded(run) {
                    let _ = installer.remove().await;
                    return Err(ServerError::Killed);
                }
                return Err(e.into());
            }
        };
        if let Err(e) = installer.start().await {
            if self.superseded(run) {
                let _ = installer.remove().await;
                return Err(ServerError::Killed);
            }
            return Err(e.into());
        }
        let mut output = attach.output;
        let mut split = LineSplitter::default();
        while let Some(Ok(item)) = output.next().await {
            if self.superseded(run) {
                break;
            }
            let bytes = match item {
                LogOutput::StdOut { message }
                | LogOutput::StdErr { message }
                | LogOutput::Console { message } => message,
                _ => continue,
            };
            for line in split.push(&bytes) {
                if let Some(f) = log.as_mut() {
                    let _ = writeln!(f, "{line}");
                }
                self.publish(ServerEvent::InstallOutput(line));
            }
        }
        if let Some(line) = split.flush() {
            self.publish(ServerEvent::InstallOutput(line));
        }

        // Removing the installer here also covers a kill that raced with the stream.
        let exit = installer.wait_for_exit().await;
        let _ = installer.remove().await;
        let _ = std::fs::remove_dir_all(&script_dir);
        if self.superseded(run) {
            return Err(ServerError::Killed);
        }

        let root = self.fs.root_path().to_path_buf();
        let (uid, gid) = (self.fs.uid(), self.fs.gid());
        if rustix::process::geteuid().is_root() {
            tokio::task::spawn_blocking(move || chown_recursive(&root, uid, gid))
                .await
                .map_err(anyhow::Error::from)?;
        } else if let Err(e) = crate::environment::chown_volume_in_container(
            self.env.docker(),
            &script.container_image,
            &format!("{}_chown", self.uuid()),
            &root,
            uid,
            gid,
        )
        .await
        {
            tracing::warn!(server = %self.uuid(), error = %e, "could not fix file ownership after install");
        }
        self.fs.disk_usage(true).await;

        if exit.exit_code != 0 {
            return Err(ServerError::Other(anyhow::anyhow!(
                "installation script exited with code {}",
                exit.exit_code
            )));
        }
        Ok(())
    }
}

fn chown_recursive(root: &std::path::Path, uid: u32, gid: u32) {
    for entry in walkdir::WalkDir::new(root).follow_links(false).into_iter().flatten() {
        if let Err(e) = std::os::unix::fs::lchown(entry.path(), Some(uid), Some(gid)) {
            tracing::debug!(path = %entry.path().display(), error = %e, "chown failed");
        }
    }
}
