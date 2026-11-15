pub mod handler;

use crate::context::AppRef;
use crate::remote::{RemoteError, SftpAuthRequest};
use crate::server::Server;
use anyhow::{Context, Result};
use handler::{SessionInfo, SftpSession};
use russh::keys::{PrivateKey, ssh_key};
use russh::server::{Auth, Msg, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_FAILURES: u32 = 10;
const FAILURE_WINDOW: Duration = Duration::from_secs(300);

pub fn parse_username(username: &str) -> Option<(&str, String)> {
    let (user, short) = username.rsplit_once('.')?;
    if user.is_empty() || short.len() != 8 || !short.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some((user, short.to_ascii_lowercase()))
}

#[derive(Default)]
pub struct AuthLimiter {
    failures: Mutex<HashMap<IpAddr, (u32, Instant)>>,
}

impl AuthLimiter {
    pub fn blocked(&self, ip: IpAddr) -> bool {
        let mut m = self.failures.lock().unwrap();
        match m.get(&ip) {
            Some((_, since)) if since.elapsed() > FAILURE_WINDOW => {
                m.remove(&ip);
                false
            }
            Some((n, _)) => *n >= MAX_FAILURES,
            None => false,
        }
    }

    pub fn fail(&self, ip: IpAddr) {
        let mut m = self.failures.lock().unwrap();
        if m.len() > 10_000 {
            m.retain(|_, (_, t)| t.elapsed() < FAILURE_WINDOW);
        }
        let e = m.entry(ip).or_insert((0, Instant::now()));
        if e.1.elapsed() > FAILURE_WINDOW {
            *e = (0, Instant::now());
        }
        e.0 += 1;
    }

    pub fn success(&self, ip: IpAddr) {
        self.failures.lock().unwrap().remove(&ip);
    }
}

fn load_or_create_host_key(path: &Path) -> Result<PrivateKey> {
    if path.exists() {
        return PrivateKey::read_openssh_file(path).context("reading sftp host key");
    }
    let dir = path.parent().context("host key path has no parent")?;
    std::fs::create_dir_all(dir)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let seed: [u8; 32] = rand::random();
    let key = PrivateKey::from(ssh_key::private::Ed25519Keypair::from_seed(&seed));
    key.write_openssh_file(path, ssh_key::LineEnding::LF)
        .context("writing sftp host key")?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    tracing::info!(path = %path.display(), "generated new sftp host key");
    Ok(key)
}

struct SshServer {
    app: AppRef,
    limiter: Arc<AuthLimiter>,
}

impl russh::server::Server for SshServer {
    type Handler = SshHandler;

    fn new_client(&mut self, peer: Option<SocketAddr>) -> SshHandler {
        SshHandler {
            app: self.app.clone(),
            limiter: self.limiter.clone(),
            ip: peer.map(|p| p.ip()).unwrap_or(IpAddr::from([0, 0, 0, 0])),
            session: None,
            channels: HashMap::new(),
        }
    }

    fn handle_session_error(&mut self, error: anyhow::Error) {
        tracing::debug!(error = %error, "sftp session error");
    }
}

struct Authed {
    server: Arc<Server>,
    info: SessionInfo,
}

struct SshHandler {
    app: AppRef,
    limiter: Arc<AuthLimiter>,
    ip: IpAddr,
    session: Option<Authed>,
    channels: HashMap<ChannelId, Channel<Msg>>,
}

impl SshHandler {
    async fn authenticate(&mut self, username: &str, kind: &str, secret: String) -> Auth {
        let Some((_, short)) = parse_username(username) else {
            return Auth::reject();
        };
        if self.limiter.blocked(self.ip) {
            tracing::warn!(ip = %self.ip, "sftp: too many failed logins");
            return Auth::reject();
        }
        let req = SftpAuthRequest {
            kind: kind.into(),
            username: username.into(),
            password: secret,
            ip: self.ip.to_string(),
        };
        let resp = match self.app.remote.validate_sftp_credentials(&req).await {
            Ok(r) => r,
            Err(e) => {
                if !RemoteError::is_client_error(&e) {
                    tracing::warn!(error = %e, "sftp: panel credential check failed");
                }
                self.limiter.fail(self.ip);
                return Auth::reject();
            }
        };
        let server = match self.app.manager.get(&resp.server) {
            Some(s) if s.uuid().to_ascii_lowercase().starts_with(&short) && !s.is_suspended() => s,
            _ => {
                self.limiter.fail(self.ip);
                return Auth::reject();
            }
        };
        self.limiter.success(self.ip);
        tracing::info!(server = %server.uuid(), user = %resp.user, ip = %self.ip, "sftp login");
        self.session = Some(Authed {
            server,
            info: SessionInfo {
                user_uuid: resp.user,
                ip: self.ip.to_string(),
                permissions: resp.permissions,
                read_only: self.app.config.system.sftp.read_only,
            },
        });
        Auth::Accept
    }
}

impl russh::server::Handler for SshHandler {
    type Error = anyhow::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(self.authenticate(user, "password", password.to_string()).await)
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        key: russh::server::VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        let Ok(openssh) = key.public_key.to_openssh() else {
            return Ok(Auth::reject());
        };
        Ok(self.authenticate(user, "public_key", openssh).await)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.session.is_none() || self.channels.len() >= 4 {
            return Ok(());
        }
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel_id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let (Some(auth), true) = (&self.session, name == "sftp") else {
            session.channel_failure(channel_id)?;
            return Ok(());
        };
        let Some(channel) = self.channels.remove(&channel_id) else {
            session.channel_failure(channel_id)?;
            return Ok(());
        };
        session.channel_success(channel_id)?;
        let sftp = SftpSession::new(auth.server.fs.clone(), Some(auth.server.clone()), auth.info.clone());
        russh_sftp::server::run(channel.into_stream(), sftp).await;
        Ok(())
    }

    async fn shell_request(&mut self, channel: ChannelId, session: &mut Session) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        session.close(channel)?;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        _data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        session.close(channel)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        Ok(())
    }
}

pub async fn run(app: AppRef) -> Result<()> {
    let cfg = &app.config.system.sftp;
    let key = load_or_create_host_key(&app.config.sftp_host_key())?;
    let config = Arc::new(russh::server::Config {
        server_id: russh::SshId::Standard(format!("SSH-2.0-wings-rs_{}", env!("CARGO_PKG_VERSION")).into()),
        methods: MethodSet::from(&[MethodKind::Password, MethodKind::PublicKey][..]),
        auth_rejection_time: Duration::from_secs(2),
        auth_rejection_time_initial: Some(Duration::from_millis(0)),
        max_auth_attempts: 6,
        inactivity_timeout: Some(Duration::from_secs(600)),
        keepalive_interval: Some(Duration::from_secs(60)),
        keys: vec![key],
        ..Default::default()
    });
    let addr = format!("{}:{}", cfg.bind_address, cfg.bind_port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding sftp listener on {addr}"))?;
    tracing::info!(%addr, "SFTP listening");
    let mut server = SshServer {
        app: app.clone(),
        limiter: Arc::new(AuthLimiter::default()),
    };
    use russh::server::Server as _;
    server.run_on_socket(config, &listener).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames() {
        assert_eq!(parse_username("steve.8d3f2a1c"), Some(("steve", "8d3f2a1c".into())));
        assert_eq!(parse_username("first.last.8D3F2A1C"), Some(("first.last", "8d3f2a1c".into())));
        assert_eq!(parse_username("steve"), None);
        assert_eq!(parse_username("steve.8d3f"), None);
        assert_eq!(parse_username("steve.zzzzzzzz"), None);
        assert_eq!(parse_username(".8d3f2a1c"), None);
    }

    #[test]
    fn limiter_blocks_after_failures() {
        let l = AuthLimiter::default();
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        for _ in 0..MAX_FAILURES {
            assert!(!l.blocked(ip));
            l.fail(ip);
        }
        assert!(l.blocked(ip));
        l.success(ip);
        assert!(!l.blocked(ip));
    }

    #[test]
    fn host_key_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join(".sftp/id_ed25519");
        let k1 = load_or_create_host_key(&p).unwrap();
        let k2 = load_or_create_host_key(&p).unwrap();
        assert_eq!(k1.public_key(), k2.public_key());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
