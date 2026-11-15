pub mod stats;

use crate::config::{Config, PullPolicy};
use crate::server::configuration::ServerConfigurationResponse;
use anyhow::{Context, Result};
use bollard::{
    Docker,
    container::AttachContainerResults,
    errors::Error as DockerError,
    models::{
        ContainerCreateBody, HostConfig, HostConfigLogConfig, Ipam, IpamConfig, Mount,
        MountType, NetworkCreateRequest, PortBinding,
    },
    query_parameters::{
        AttachContainerOptionsBuilder, CreateContainerOptionsBuilder, CreateImageOptionsBuilder,
        KillContainerOptionsBuilder, RemoveContainerOptionsBuilder, StatsOptionsBuilder,
        StopContainerOptionsBuilder, WaitContainerOptionsBuilder,
    },
};
use futures_util::{Stream, StreamExt};
use std::collections::HashMap;
use std::path::Path;

pub const LABEL_SERVICE: &str = "Service";
pub const LABEL_SERVICE_VALUE: &str = "Pterodactyl";
pub const LABEL_CONTAINER_TYPE: &str = "ContainerType";

pub const CONTAINER_HOME: &str = "/home/container";

#[derive(Debug, Clone, Copy)]
pub struct ExitInfo {
    pub exit_code: i64,
    pub oom_killed: bool,
}

#[derive(Clone)]
pub struct DockerEnvironment {
    docker: Docker,
    name: String,
}

pub fn is_not_found(e: &DockerError) -> bool {
    matches!(e, DockerError::DockerResponseServerError { status_code: 404, .. })
}

pub async fn ensure_network(docker: &Docker, cfg: &Config) -> Result<()> {
    let net = &cfg.docker.network;
    match docker.inspect_network(&net.name, None).await {
        Ok(_) => return Ok(()),
        Err(e) if is_not_found(&e) => {}
        Err(e) => return Err(e).context("inspecting docker network"),
    }
    tracing::info!(network = %net.name, "creating docker network");
    docker
        .create_network(NetworkCreateRequest {
            name: net.name.clone(),
            driver: Some(net.driver.clone()),
            internal: Some(false),
            attachable: Some(false),
            ipam: Some(Ipam {
                driver: Some("default".into()),
                config: Some(vec![IpamConfig {
                    subnet: Some(net.subnet.clone()),
                    gateway: Some(net.gateway.clone()),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            options: Some(HashMap::from([
                ("encryption".into(), "false".into()),
                ("com.docker.network.bridge.default_bridge".into(), "false".into()),
                ("com.docker.network.bridge.enable_icc".into(), "true".into()),
                ("com.docker.network.bridge.enable_ip_masquerade".into(), "true".into()),
                ("com.docker.network.bridge.host_binding_ipv4".into(), "0.0.0.0".into()),
                ("com.docker.network.driver.mtu".into(), "1500".into()),
            ])),
            ..Default::default()
        })
        .await
        .context("creating docker network")?;
    Ok(())
}

pub async fn pull_image(docker: &Docker, image: &str, policy: PullPolicy) -> Result<()> {
    let local = docker.inspect_image(image).await.is_ok();
    match policy {
        PullPolicy::Never if local => return Ok(()),
        PullPolicy::Never => anyhow::bail!("image {image} is not present locally and image_pull_policy is never"),
        PullPolicy::IfNotPresent if local => return Ok(()),
        _ => {}
    }
    let opts = CreateImageOptionsBuilder::new().from_image(image).build();
    let mut stream = docker.create_image(Some(opts), None, None);
    let mut pull_err = None;
    while let Some(item) = stream.next().await {
        if let Err(e) = item {
            pull_err = Some(e);
            break;
        }
    }
    match pull_err {
        None => Ok(()),
        Some(e) if local => {
            tracing::debug!(%image, error = %e, "image pull failed, using local copy");
            Ok(())
        }
        Some(e) => Err(e).with_context(|| format!("pulling image {image}")),
    }
}

pub fn port_bindings(
    cfg: &ServerConfigurationResponse,
) -> (Vec<String>, HashMap<String, Option<Vec<PortBinding>>>) {
    let mut exposed = Vec::new();
    let mut bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
    for (ip, ports) in &cfg.settings.allocations.mappings {
        for port in ports {
            for proto in ["tcp", "udp"] {
                let key = format!("{port}/{proto}");
                if !exposed.contains(&key) {
                    exposed.push(key.clone());
                }
                bindings
                    .entry(key)
                    .or_insert_with(|| Some(Vec::new()))
                    .get_or_insert_with(Vec::new)
                    .push(PortBinding {
                        host_ip: Some(ip.clone()),
                        host_port: Some(port.to_string()),
                    });
            }
        }
    }
    (exposed, bindings)
}

pub fn environment_vars(cfg: &ServerConfigurationResponse, config: &Config) -> Vec<String> {
    let s = &cfg.settings;
    let mut env: Vec<String> = vec![
        format!("TZ={}", config.system.timezone),
        format!("STARTUP={}", s.invocation),
        format!("SERVER_MEMORY={}", s.build.memory_limit),
        format!("SERVER_IP={}", s.allocations.default.ip),
        format!("SERVER_PORT={}", s.allocations.default.port),
        format!("P_SERVER_UUID={}", s.uuid),
    ];
    for (k, v) in &s.environment {
        let v = match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Null => String::new(),
            other => other.to_string(),
        };
        env.push(format!("{k}={v}"));
    }
    env
}

fn resource_limits(memory_mib: u64, swap_mib: i64, cpu: u64, config: &Config) -> HostConfig {
    let mut hc = HostConfig::default();
    if memory_mib > 0 {
        let mult = config.docker.overhead.multiplier_for(memory_mib);
        let hard = (memory_mib as f64 * 1024.0 * 1024.0 * mult) as i64;
        hc.memory = Some(hard);
        hc.memory_reservation = Some((memory_mib * 1024 * 1024) as i64);
        hc.memory_swap = Some(if swap_mib < 0 {
            -1
        } else {
            hard + swap_mib * 1024 * 1024
        });
    }
    if cpu > 0 {
        hc.cpu_period = Some(100_000);
        hc.cpu_quota = Some((cpu * 1000) as i64);
    }
    if config.docker.container_pid_limit > 0 {
        hc.pids_limit = Some(config.docker.container_pid_limit);
    }
    hc
}

impl DockerEnvironment {
    pub fn new(docker: Docker, uuid: &str) -> Self {
        Self {
            docker,
            name: uuid.to_string(),
        }
    }

    pub fn docker(&self) -> &Docker {
        &self.docker
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub async fn exists(&self) -> Result<bool> {
        match self.docker.inspect_container(&self.name, None).await {
            Ok(_) => Ok(true),
            Err(e) if is_not_found(&e) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn is_running(&self) -> Result<bool> {
        match self.docker.inspect_container(&self.name, None).await {
            Ok(c) => Ok(c.state.and_then(|s| s.running).unwrap_or(false)),
            Err(e) if is_not_found(&e) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn started_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        let c = self.docker.inspect_container(&self.name, None).await.ok()?;
        let s = c.state?;
        if s.running != Some(true) {
            return None;
        }
        chrono::DateTime::parse_from_rfc3339(s.started_at.as_deref()?)
            .ok()
            .map(|d| d.with_timezone(&chrono::Utc))
    }

    pub async fn create(
        &self,
        cfg: &ServerConfigurationResponse,
        config: &Config,
        volume: &Path,
    ) -> Result<()> {
        let s = &cfg.settings;
        pull_image(&self.docker, &s.container.image, config.docker.image_pull_policy).await?;
        self.remove().await?;

        let (exposed, bindings) = port_bindings(cfg);
        let mut hc = resource_limits(s.build.memory_limit, s.build.swap, s.build.cpu_limit, config);
        hc.cpuset_cpus = s.build.threads.clone().filter(|t| !t.is_empty());
        if config.docker.enable_blkio_weight {
            hc.blkio_weight = Some(s.build.io_weight.clamp(10, 1000));
        }
        hc.oom_kill_disable = Some(s.build.oom_disabled);
        hc.port_bindings = Some(bindings);
        hc.network_mode = Some(config.docker.network.name.clone());
        hc.dns = Some(config.docker.network.dns.clone());
        hc.tmpfs = Some(HashMap::from([(
            "/tmp".into(),
            format!("rw,exec,nosuid,size={}M", config.docker.tmpfs_size),
        )]));
        let mut mounts = vec![Mount {
            target: Some(CONTAINER_HOME.into()),
            source: Some(volume.to_string_lossy().into_owned()),
            typ: Some(MountType::BIND),
            read_only: Some(false),
            ..Default::default()
        }];
        for m in &s.mounts {
            mounts.push(Mount {
                target: Some(m.target.clone()),
                source: Some(m.source.clone()),
                typ: Some(MountType::BIND),
                read_only: Some(m.read_only),
                ..Default::default()
            });
        }
        hc.mounts = Some(mounts);
        hc.log_config = Some(HostConfigLogConfig {
            typ: Some("local".into()),
            config: Some(HashMap::from([
                ("max-size".into(), "5m".into()),
                ("max-file".into(), "1".into()),
                ("compress".into(), "false".into()),
            ])),
        });
        hc.security_opt = Some(vec!["no-new-privileges".into()]);
        hc.cap_drop = Some(
            [
                "setpcap", "mknod", "audit_write", "net_raw", "dac_override", "fowner",
                "fsetid", "net_bind_service", "sys_chroot", "setfcap",
            ]
            .map(String::from)
            .to_vec(),
        );
        hc.readonly_rootfs = Some(true);

        let body = ContainerCreateBody {
            hostname: Some(s.uuid.chars().take(12).collect()),
            user: Some(format!("{}:{}", config.system.user.uid, config.system.user.gid)),
            attach_stdin: Some(true),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            open_stdin: Some(true),
            tty: Some(true),
            exposed_ports: Some(exposed),
            image: Some(s.container.image.clone()),
            env: Some(environment_vars(cfg, config)),
            labels: Some(HashMap::from([
                (LABEL_SERVICE.into(), LABEL_SERVICE_VALUE.into()),
                (LABEL_CONTAINER_TYPE.into(), "server_process".into()),
            ])),
            host_config: Some(hc),
            ..Default::default()
        };

        let opts = CreateContainerOptionsBuilder::new().name(&self.name).build();
        match self.docker.create_container(Some(opts.clone()), body.clone()).await {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().to_lowercase().contains("blkio") => {
                tracing::warn!(error = %e, "blkio weight rejected, creating container without it");
                let mut body = body;
                if let Some(hc) = body.host_config.as_mut() {
                    hc.blkio_weight = None;
                }
                self.docker.create_container(Some(opts), body).await?;
                Ok(())
            }
            Err(e) => Err(e).context("creating container"),
        }
    }

    pub async fn attach(&self) -> Result<AttachContainerResults> {
        let opts = AttachContainerOptionsBuilder::new()
            .stdin(true)
            .stdout(true)
            .stderr(true)
            .stream(true)
            .build();
        Ok(self.docker.attach_container(&self.name, Some(opts)).await?)
    }

    pub async fn start(&self) -> Result<()> {
        self.docker
            .start_container(&self.name, None)
            .await
            .context("starting container")
    }

    pub async fn stop(&self, timeout_secs: i32) -> Result<()> {
        let opts = StopContainerOptionsBuilder::new().t(timeout_secs).build();
        match self.docker.stop_container(&self.name, Some(opts)).await {
            Ok(()) => Ok(()),
            Err(DockerError::DockerResponseServerError { status_code: 304 | 404, .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn kill(&self, signal: &str) -> Result<()> {
        let opts = KillContainerOptionsBuilder::new().signal(signal).build();
        match self.docker.kill_container(&self.name, Some(opts)).await {
            Ok(()) => Ok(()),
            Err(DockerError::DockerResponseServerError { status_code: 404 | 409, .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn remove(&self) -> Result<()> {
        let opts = RemoveContainerOptionsBuilder::new().force(true).v(true).build();
        match self.docker.remove_container(&self.name, Some(opts)).await {
            Ok(()) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) => Err(e).context("removing container"),
        }
    }

    pub async fn wait_for_exit(&self) -> ExitInfo {
        let opts = WaitContainerOptionsBuilder::new().condition("not-running").build();
        let _ = self.docker.wait_container(&self.name, Some(opts)).next().await;
        self.exit_info().await
    }

    pub async fn exit_info(&self) -> ExitInfo {
        match self.docker.inspect_container(&self.name, None).await {
            Ok(c) => {
                let s = c.state.unwrap_or_default();
                ExitInfo {
                    exit_code: s.exit_code.unwrap_or(0),
                    oom_killed: s.oom_killed.unwrap_or(false),
                }
            }
            Err(_) => ExitInfo {
                exit_code: 0,
                oom_killed: false,
            },
        }
    }

    pub fn stats_stream(
        &self,
    ) -> impl Stream<Item = Result<bollard::models::ContainerStatsResponse, DockerError>> + use<> {
        let opts = StatsOptionsBuilder::new().stream(true).build();
        self.docker.stats(&self.name, Some(opts))
    }
}

pub async fn chown_volume_in_container(
    docker: &Docker,
    image: &str,
    name: &str,
    volume: &Path,
    uid: u32,
    gid: u32,
) -> Result<()> {
    let env = DockerEnvironment::new(docker.clone(), name);
    env.remove().await?;
    let body = ContainerCreateBody {
        image: Some(image.to_string()),
        user: Some("0:0".into()),
        entrypoint: Some(vec!["chown".into()]),
        cmd: Some(vec!["-R".into(), format!("{uid}:{gid}"), "/mnt/server".into()]),
        labels: Some(HashMap::from([
            (LABEL_SERVICE.into(), LABEL_SERVICE_VALUE.into()),
            (LABEL_CONTAINER_TYPE.into(), "server_chown".into()),
        ])),
        host_config: Some(HostConfig {
            network_mode: Some("none".into()),
            mounts: Some(vec![Mount {
                target: Some("/mnt/server".into()),
                source: Some(volume.to_string_lossy().into_owned()),
                typ: Some(MountType::BIND),
                read_only: Some(false),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let opts = CreateContainerOptionsBuilder::new().name(name).build();
    docker.create_container(Some(opts), body).await?;
    env.start().await?;
    let exit = env.wait_for_exit().await;
    env.remove().await?;
    anyhow::ensure!(exit.exit_code == 0, "chown container exited with {}", exit.exit_code);
    Ok(())
}

pub struct InstallerSpec<'a> {
    pub name: String,
    pub image: &'a str,
    pub entrypoint: &'a str,
    pub env: Vec<String>,
    pub server_volume: &'a Path,
    pub script_dir: &'a Path,
    pub memory_mib: u64,
    pub cpu: u64,
}

impl DockerEnvironment {
    pub async fn create_installer(docker: &Docker, spec: &InstallerSpec<'_>, config: &Config) -> Result<()> {
        pull_image(docker, spec.image, config.docker.image_pull_policy).await?;
        let env = DockerEnvironment::new(docker.clone(), &spec.name);
        env.remove().await?;
        let mut hc = resource_limits(spec.memory_mib, -1, spec.cpu, config);
        hc.network_mode = Some(config.docker.network.name.clone());
        hc.dns = Some(config.docker.network.dns.clone());
        hc.tmpfs = Some(HashMap::from([(
            "/tmp".into(),
            format!("rw,exec,nosuid,size={}M", config.docker.tmpfs_size),
        )]));
        hc.mounts = Some(vec![
            Mount {
                target: Some("/mnt/server".into()),
                source: Some(spec.server_volume.to_string_lossy().into_owned()),
                typ: Some(MountType::BIND),
                read_only: Some(false),
                ..Default::default()
            },
            Mount {
                target: Some("/mnt/install".into()),
                source: Some(spec.script_dir.to_string_lossy().into_owned()),
                typ: Some(MountType::BIND),
                read_only: Some(false),
                ..Default::default()
            },
        ]);
        hc.log_config = Some(HostConfigLogConfig {
            typ: Some("local".into()),
            config: Some(HashMap::from([("max-size".into(), "5m".into())])),
        });
        let body = ContainerCreateBody {
            hostname: Some("installer".into()),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            attach_stdin: Some(true),
            open_stdin: Some(true),
            tty: Some(true),
            image: Some(spec.image.to_string()),
            env: Some(spec.env.clone()),
            cmd: Some(vec![spec.entrypoint.to_string(), "/mnt/install/install.sh".into()]),
            labels: Some(HashMap::from([
                (LABEL_SERVICE.into(), LABEL_SERVICE_VALUE.into()),
                (LABEL_CONTAINER_TYPE.into(), "server_installer".into()),
            ])),
            host_config: Some(hc),
            ..Default::default()
        };
        let opts = CreateContainerOptionsBuilder::new().name(&spec.name).build();
        docker
            .create_container(Some(opts), body)
            .await
            .context("creating installer container")?;
        Ok(())
    }
}
