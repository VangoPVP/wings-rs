pub use crate::protocol::server::{NetworkUsage, PowerAction, PowerState, ResourceUsage};

#[derive(Clone, Debug)]
pub enum ServerEvent {
    ConsoleOutput(String),
    InstallOutput(String),
    InstallStarted,
    InstallCompleted { successful: bool },
    Status(PowerState),
    Stats(ResourceUsage),
    DaemonMessage(String),
    BackupCompleted(serde_json::Value),
    BackupRestoreCompleted,
    TransferLogs(String),
    TransferStatus(String),
    Deleted,
}

impl ServerEvent {
    pub fn to_ws(&self) -> (&'static str, Vec<String>) {
        match self {
            ServerEvent::ConsoleOutput(l) => ("console output", vec![l.clone()]),
            ServerEvent::InstallOutput(l) => ("install output", vec![l.clone()]),
            ServerEvent::InstallStarted => ("install started", vec![]),
            ServerEvent::InstallCompleted { successful } => {
                ("install completed", vec![successful.to_string()])
            }
            ServerEvent::Status(s) => ("status", vec![s.as_str().into()]),
            ServerEvent::Stats(u) => ("stats", vec![serde_json::to_string(u).unwrap_or_default()]),
            ServerEvent::DaemonMessage(m) => ("daemon message", vec![m.clone()]),
            ServerEvent::BackupCompleted(v) => ("backup completed", vec![v.to_string()]),
            ServerEvent::BackupRestoreCompleted => ("backup restore completed", vec![]),
            ServerEvent::TransferLogs(l) => ("transfer logs", vec![l.clone()]),
            ServerEvent::TransferStatus(s) => ("transfer status", vec![s.clone()]),
            ServerEvent::Deleted => ("deleted", vec![]),
        }
    }

    pub fn required_permission(&self) -> Option<&'static str> {
        match self {
            ServerEvent::InstallOutput(_)
            | ServerEvent::InstallStarted
            | ServerEvent::InstallCompleted { .. } => Some("admin.websocket.install"),
            ServerEvent::TransferLogs(_) | ServerEvent::TransferStatus(_) => {
                Some("admin.websocket.transfer")
            }
            ServerEvent::BackupCompleted(_) | ServerEvent::BackupRestoreCompleted => {
                Some("backup.read")
            }
            _ => None,
        }
    }
}
