use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(utoipa::ToSchema)]
pub enum PowerState {
    Offline,
    Starting,
    Running,
    Stopping,
}

impl PowerState {
    pub fn as_str(&self) -> &'static str {
        match self {
            PowerState::Offline => "offline",
            PowerState::Starting => "starting",
            PowerState::Running => "running",
            PowerState::Stopping => "stopping",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(utoipa::ToSchema)]
pub enum PowerAction {
    Start,
    Stop,
    Restart,
    Kill,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct NetworkUsage {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[derive(utoipa::ToSchema)]
pub struct ResourceUsage {
    pub memory_bytes: u64,
    pub memory_limit_bytes: u64,
    pub cpu_absolute: f64,
    pub network: NetworkUsage,
    pub uptime: u64,
    pub state: PowerState,
    pub disk_bytes: u64,
}

impl Default for ResourceUsage {
    fn default() -> Self {
        Self {
            memory_bytes: 0,
            memory_limit_bytes: 0,
            cpu_absolute: 0.0,
            network: NetworkUsage::default(),
            uptime: 0,
            state: PowerState::Offline,
            disk_bytes: 0,
        }
    }
}

