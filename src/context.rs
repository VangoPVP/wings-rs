use crate::activity::ActivityLog;
use crate::config::Config;
use crate::jwt::JwtValidator;
use crate::remote::RemoteClient;
use crate::server::manager::Manager;
use bollard::Docker;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Notify;

pub struct Ctx {
    pub config: Arc<Config>,
    pub docker: Docker,
    pub remote: RemoteClient,
    pub activity: ActivityLog,
    pub states_changed: Notify,
}

pub struct App {
    pub ctx: Arc<Ctx>,
    pub manager: Manager,
    pub jwt: JwtValidator,
    pub started_at: Instant,
}

impl Deref for App {
    type Target = Ctx;
    fn deref(&self) -> &Ctx {
        &self.ctx
    }
}

pub type AppRef = Arc<App>;
