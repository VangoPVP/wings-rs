use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct ServerConfigurationResponse {
    pub settings: ServerSettings,
    pub process_configuration: ProcessConfiguration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct ServerSettings {
    pub uuid: String,
    #[serde(default)]
    pub meta: ServerMeta,
    #[serde(default)]
    pub suspended: bool,
    #[serde(default)]
    pub invocation: String,
    #[serde(default)]
    pub skip_egg_scripts: bool,
    #[serde(default)]
    pub environment: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub allocations: Allocations,
    #[serde(default)]
    pub build: BuildLimits,
    #[serde(default = "default_true")]
    pub crash_detection_enabled: bool,
    #[serde(default)]
    pub mounts: Vec<Mount>,
    #[serde(default)]
    pub egg: EggSettings,
    pub container: ContainerSettings,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct ServerMeta {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct Allocations {
    #[serde(default)]
    pub force_outgoing_ip: bool,
    #[serde(default)]
    pub default: Allocation,
    #[serde(default)]
    pub mappings: HashMap<String, Vec<u16>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct Allocation {
    pub ip: String,
    pub port: u16,
}

impl Default for Allocation {
    fn default() -> Self {
        Self {
            ip: "0.0.0.0".into(),
            port: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct BuildLimits {
    #[serde(default)]
    pub memory_limit: u64,
    #[serde(default)]
    pub swap: i64,
    #[serde(default = "default_io")]
    pub io_weight: u16,
    #[serde(default)]
    pub cpu_limit: u64,
    #[serde(default)]
    pub threads: Option<String>,
    #[serde(default)]
    pub disk_space: u64,
    #[serde(default)]
    pub oom_disabled: bool,
}

fn default_io() -> u16 {
    500
}

impl Default for BuildLimits {
    fn default() -> Self {
        Self {
            memory_limit: 0,
            swap: 0,
            io_weight: default_io(),
            cpu_limit: 0,
            threads: None,
            disk_space: 0,
            oom_disabled: false,
        }
    }
}

impl BuildLimits {
    pub fn disk_bytes(&self) -> u64 {
        self.disk_space * 1024 * 1024
    }
    pub fn memory_bytes(&self) -> u64 {
        self.memory_limit * 1024 * 1024
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct Mount {
    pub source: String,
    pub target: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct EggSettings {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub file_denylist: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct ContainerSettings {
    pub image: String,
    #[serde(default)]
    pub requires_rebuild: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct ProcessConfiguration {
    #[serde(default)]
    pub startup: StartupConfiguration,
    #[serde(default)]
    pub stop: StopConfiguration,
    #[serde(default)]
    pub configs: Vec<ConfigurationFile>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct StartupConfiguration {
    #[serde(default)]
    pub done: Vec<String>,
    #[serde(default)]
    pub user_interaction: Vec<String>,
    #[serde(default)]
    pub strip_ansi: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct StopConfiguration {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub value: String,
}

impl Default for StopConfiguration {
    fn default() -> Self {
        Self {
            kind: "stop".into(),
            value: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct ConfigurationFile {
    pub file: String,
    pub parser: String,
    #[serde(default)]
    pub replace: Vec<ConfigurationReplacement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct ConfigurationReplacement {
    #[serde(rename = "match")]
    pub match_key: String,
    #[serde(default)]
    pub if_value: Option<String>,
    pub replace_with: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct InstallationScript {
    pub container_image: String,
    pub entrypoint: String,
    pub script: String,
}
