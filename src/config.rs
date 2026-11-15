use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_CONFIG_PATH: &str = "/etc/pterodactyl/config.yml";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub debug: bool,
    pub uuid: String,
    pub token_id: String,
    pub token: String,
    pub api: ApiConfig,
    pub system: SystemConfig,
    #[serde(default)]
    pub docker: DockerConfig,
    pub remote: String,
    #[serde(default)]
    pub remote_query: RemoteQueryConfig,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_api_port")]
    pub port: u16,
    #[serde(default)]
    pub ssl: SslConfig,
    #[serde(default = "default_upload_limit")]
    pub upload_limit: u64,
    #[serde(default)]
    pub docs: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SslConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub cert: PathBuf,
    #[serde(default)]
    pub key: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemConfig {
    #[serde(default = "default_root_dir")]
    pub root_directory: PathBuf,
    #[serde(default = "default_data_dir")]
    pub data: PathBuf,
    #[serde(default = "default_archive_dir")]
    pub archive_directory: PathBuf,
    #[serde(default = "default_backup_dir")]
    pub backup_directory: PathBuf,
    #[serde(default = "default_tmp_dir")]
    pub tmp_directory: PathBuf,
    #[serde(default)]
    pub user: SystemUser,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    #[serde(default = "default_disk_check_interval")]
    pub disk_check_interval: u64,
    #[serde(default = "default_ws_log_count")]
    pub websocket_log_count: usize,
    #[serde(default = "default_state_report_interval")]
    pub state_report_interval: u64,
    #[serde(default)]
    pub crash_detection: CrashDetectionConfig,
    #[serde(default)]
    pub sftp: SftpConfig,
    #[serde(default)]
    pub backups: BackupConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SystemUser {
    pub uid: u32,
    pub gid: u32,
}

impl Default for SystemUser {
    fn default() -> Self {
        Self { uid: 988, gid: 988 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CrashDetectionConfig {
    pub enabled: bool,
    pub detect_clean_exit_as_crash: bool,
    pub timeout: u64,
}

impl Default for CrashDetectionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            detect_clean_exit_as_crash: true,
            timeout: 60,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SftpConfig {
    pub enabled: bool,
    pub bind_address: String,
    pub bind_port: u16,
    pub read_only: bool,
}

impl Default for SftpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bind_address: "0.0.0.0".into(),
            bind_port: 2022,
            read_only: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BackupConfig {
    pub compression_level: u32,
    pub write_limit: u64,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            compression_level: 6,
            write_limit: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DockerConfig {
    pub network: DockerNetwork,
    pub container_pid_limit: i64,
    pub overhead: OverheadConfig,
    pub installer_limits: InstallerLimits,
    pub tmpfs_size: u64,
    pub enable_blkio_weight: bool,
    pub image_pull_policy: PullPolicy,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullPolicy {
    #[default]
    Always,
    IfNotPresent,
    Never,
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self {
            network: DockerNetwork::default(),
            container_pid_limit: 512,
            overhead: OverheadConfig::default(),
            installer_limits: InstallerLimits::default(),
            tmpfs_size: 100,
            enable_blkio_weight: true,
            image_pull_policy: PullPolicy::Always,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DockerNetwork {
    pub name: String,
    pub interface: String,
    pub subnet: String,
    pub gateway: String,
    pub driver: String,
    pub dns: Vec<String>,
}

impl Default for DockerNetwork {
    fn default() -> Self {
        Self {
            name: "pterodactyl_nw".into(),
            interface: "172.18.0.1".into(),
            subnet: "172.18.0.0/16".into(),
            gateway: "172.18.0.1".into(),
            driver: "bridge".into(),
            dns: vec!["1.1.1.1".into(), "1.0.0.1".into()],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OverheadConfig {
    pub default_multiplier: f64,
    pub multipliers: Vec<(u64, f64)>,
}

impl Default for OverheadConfig {
    fn default() -> Self {
        Self {
            default_multiplier: 1.05,
            multipliers: vec![(2048, 1.15), (4096, 1.10)],
        }
    }
}

impl OverheadConfig {
    pub fn multiplier_for(&self, memory_mib: u64) -> f64 {
        let mut rules = self.multipliers.clone();
        rules.sort_by_key(|r| r.0);
        rules
            .iter()
            .find(|(threshold, _)| memory_mib <= *threshold)
            .map(|(_, m)| *m)
            .unwrap_or(self.default_multiplier)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct InstallerLimits {
    pub memory: u64,
    pub cpu: u64,
}

impl Default for InstallerLimits {
    fn default() -> Self {
        Self {
            memory: 1024,
            cpu: 100,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteQueryConfig {
    pub timeout: u64,
    pub boot_servers_per_page: u32,
}

impl Default for RemoteQueryConfig {
    fn default() -> Self {
        Self {
            timeout: 30,
            boot_servers_per_page: 50,
        }
    }
}

fn default_host() -> String {
    "0.0.0.0".into()
}
fn default_api_port() -> u16 {
    8080
}
fn default_upload_limit() -> u64 {
    100
}
fn default_root_dir() -> PathBuf {
    "/var/lib/pterodactyl".into()
}
fn default_data_dir() -> PathBuf {
    "/var/lib/pterodactyl/volumes".into()
}
fn default_archive_dir() -> PathBuf {
    "/var/lib/pterodactyl/archives".into()
}
fn default_backup_dir() -> PathBuf {
    "/var/lib/pterodactyl/backups".into()
}
fn default_tmp_dir() -> PathBuf {
    "/tmp/pterodactyl".into()
}
fn default_timezone() -> String {
    "UTC".into()
}
fn default_disk_check_interval() -> u64 {
    150
}
fn default_ws_log_count() -> usize {
    150
}
fn default_state_report_interval() -> u64 {
    30
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let mut cfg: Config = serde_saphyr::from_str(&raw)
            .with_context(|| format!("parsing config file {}", path.display()))?;
        cfg.remote = cfg.remote.trim_end_matches('/').to_string();
        cfg.make_paths_absolute()?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn make_paths_absolute(&mut self) -> Result<()> {
        let cwd = std::env::current_dir()?;
        let s = &mut self.system;
        for p in [
            &mut s.root_directory,
            &mut s.data,
            &mut s.archive_directory,
            &mut s.backup_directory,
            &mut s.tmp_directory,
        ] {
            if p.is_relative() {
                *p = cwd.join(&*p);
            }
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(!self.token.is_empty(), "config: `token` must not be empty");
        anyhow::ensure!(!self.token_id.is_empty(), "config: `token_id` must not be empty");
        anyhow::ensure!(!self.remote.is_empty(), "config: `remote` must not be empty");
        if self.api.ssl.enabled {
            anyhow::ensure!(
                self.api.ssl.cert.exists() && self.api.ssl.key.exists(),
                "config: api.ssl enabled but cert/key file missing"
            );
        }
        Ok(())
    }

    pub fn ensure_directories(&self) -> Result<()> {
        for dir in [
            &self.system.root_directory,
            &self.system.data,
            &self.system.archive_directory,
            &self.system.backup_directory,
            &self.system.tmp_directory,
        ] {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating directory {}", dir.display()))?;
        }
        Ok(())
    }

    pub fn states_file(&self) -> PathBuf {
        self.system.root_directory.join("states.json")
    }

    pub fn sftp_host_key(&self) -> PathBuf {
        self.system.root_directory.join(".sftp").join("id_ed25519")
    }

    pub fn server_volume(&self, uuid: &str) -> PathBuf {
        self.system.data.join(uuid)
    }

    pub fn upload_limit_bytes(&self) -> u64 {
        self.api.upload_limit * 1024 * 1024
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overhead_rules() {
        let o = OverheadConfig::default();
        assert_eq!(o.multiplier_for(1024), 1.15);
        assert_eq!(o.multiplier_for(2048), 1.15);
        assert_eq!(o.multiplier_for(3000), 1.10);
        assert_eq!(o.multiplier_for(8192), 1.05);
    }

    #[test]
    fn minimal_yaml_parses() {
        let yaml = r#"
uuid: node-1
token_id: abc
token: secret
remote: http://localhost:8081/
api:
  port: 8080
system:
  data: /tmp/v
"#;
        let mut c: Config = serde_saphyr::from_str(yaml).unwrap();
        c.remote = c.remote.trim_end_matches('/').to_string();
        assert_eq!(c.remote, "http://localhost:8081");
        assert_eq!(c.system.sftp.bind_port, 2022);
        assert_eq!(c.docker.container_pid_limit, 512);
        assert!(!c.api.docs, "docs must be opt-in");
    }
}
