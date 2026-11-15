use super::ServerRef;
use crate::context::AppRef;
use crate::error::ApiError;
use crate::jwt::{JwtError, WebsocketClaims};
use crate::server::Server;
use crate::server::events::{PowerAction, ServerEvent};
use axum::{
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::HeaderMap,
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};

const EXPIRY_WARNING_SECS: u64 = 60;
const MAX_MSGS_PER_SEC: u32 = 20;
const AUTH_DEADLINE: Duration = Duration::from_secs(30);

pub use crate::protocol::wings_api::WsMessage;

trait WsMessageExt {
    fn new(event: &str, args: Vec<String>) -> Self;
    fn text(&self) -> Message;
}

impl WsMessageExt for WsMessage {
    fn new(event: &str, args: Vec<String>) -> Self {
        Self {
            event: event.to_string(),
            args,
        }
    }
    fn text(&self) -> Message {
        Message::Text(serde_json::to_string(self).unwrap_or_default().into())
    }
}

pub async fn handler(
    State(app): State<AppRef>,
    ServerRef(server): ServerRef,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    check_origin(&app, &headers)?;
    Ok(ws
        .max_message_size(64 * 1024)
        .on_upgrade(move |socket| Connection::new(app, server).run(socket)))
}

fn check_origin(app: &AppRef, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) else {
        return Ok(());
    };
    let cfg = &app.config;
    let ok = origin == cfg.remote
        || cfg.allowed_origins.iter().any(|o| o == "*" || o == origin);
    if ok {
        Ok(())
    } else {
        Err(ApiError::forbidden("origin not allowed"))
    }
}

struct Connection {
    app: AppRef,
    server: Arc<Server>,
    claims: Option<WebsocketClaims>,
    expiring_sent: bool,
    out_tx: mpsc::Sender<WsMessage>,
    out_rx: mpsc::Receiver<WsMessage>,
    window_start: Instant,
    window_count: u32,
}

enum Flow {
    Continue,
    Close,
}

impl Connection {
    fn new(app: AppRef, server: Arc<Server>) -> Self {
        let (out_tx, out_rx) = mpsc::channel(32);
        Self {
            app,
            server,
            claims: None,
            expiring_sent: false,
            out_tx,
            out_rx,
            window_start: Instant::now(),
            window_count: 0,
        }
    }

    fn can(&self, perm: &str) -> bool {
        self.claims.as_ref().is_some_and(|c| c.has_permission(perm))
    }

    async fn run(mut self, socket: WebSocket) {
        let (mut tx, mut rx) = socket.split();
        let mut events = self.server.subscribe();
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        let opened = Instant::now();

        loop {
            let outgoing: Option<WsMessage> = tokio::select! {
                msg = rx.next() => match msg {
                    Some(Ok(Message::Text(t))) => {
                        match self.on_message(&t).await {
                            (Flow::Close, m) => {
                                if let Some(m) = m { let _ = tx.send(m.text()).await; }
                                break;
                            }
                            (Flow::Continue, m) => m,
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => None,
                },
                ev = events.recv() => match ev {
                    Ok(ServerEvent::Deleted) => {
                        let (n, a) = ServerEvent::Deleted.to_ws();
                        let _ = tx.send(WsMessage::new(n, a).text()).await;
                        break;
                    }
                    Ok(ev) => self.forward(&ev),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!(server = %self.server.uuid(), skipped = n, "websocket lagged");
                        None
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                Some(m) = self.out_rx.recv() => Some(m),
                _ = tick.tick() => {
                    if self.claims.is_none() && opened.elapsed() > AUTH_DEADLINE {
                        let _ = tx.send(WsMessage::new("jwt error", vec!["authentication timeout".into()]).text()).await;
                        break;
                    }
                    self.check_token()
                }
            };
            if let Some(m) = outgoing
                && tx.send(m.text()).await.is_err()
            {
                break;
            }
        }
        let _ = tx.close().await;
    }

    fn forward(&self, ev: &ServerEvent) -> Option<WsMessage> {
        self.claims.as_ref()?;
        if let Some(p) = ev.required_permission()
            && !self.can(p)
        {
            return None;
        }
        let (name, args) = ev.to_ws();
        Some(WsMessage::new(name, args))
    }

    fn check_token(&mut self) -> Option<WsMessage> {
        let claims = self.claims.as_ref()?;
        if let Some(jti) = &claims.base.jti
            && self.app.jwt.is_denied(jti)
        {
            self.claims = None;
            return Some(WsMessage::new("jwt error", vec!["jwt: token has been revoked".into()]));
        }
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        if now >= claims.base.exp {
            self.claims = None;
            return Some(WsMessage::new("token expired", vec![]));
        }
        if !self.expiring_sent && claims.base.exp - now <= EXPIRY_WARNING_SECS {
            self.expiring_sent = true;
            return Some(WsMessage::new("token expiring", vec![]));
        }
        None
    }

    fn throttled(&mut self) -> bool {
        if self.window_start.elapsed() >= Duration::from_secs(1) {
            self.window_start = Instant::now();
            self.window_count = 0;
        }
        self.window_count += 1;
        self.window_count > MAX_MSGS_PER_SEC
    }

    async fn on_message(&mut self, raw: &str) -> (Flow, Option<WsMessage>) {
        if self.throttled() {
            return (
                Flow::Continue,
                Some(WsMessage::new("daemon error", vec!["too many messages, slow down".into()])),
            );
        }
        let Ok(msg) = serde_json::from_str::<WsMessage>(raw) else {
            return (
                Flow::Continue,
                Some(WsMessage::new("daemon error", vec!["invalid message format".into()])),
            );
        };

        if msg.event == "auth" {
            return self.on_auth(msg.args.first().map(String::as_str).unwrap_or(""));
        }
        if self.claims.is_none() {
            return (
                Flow::Continue,
                Some(WsMessage::new("jwt error", vec!["you must authenticate first".into()])),
            );
        }

        let reply = match msg.event.as_str() {
            "set state" => self.on_set_state(msg.args.first().map(String::as_str)),
            "send command" => self.on_command(msg.args.first().cloned()).await,
            "send logs" => {
                if self.can("control.console") {
                    for line in self.server.console_history(self.app.config.system.websocket_log_count) {
                        let _ = self.out_tx.try_send(WsMessage::new("console output", vec![line]));
                    }
                }
                None
            }
            "send stats" => Some(WsMessage::new(
                "stats",
                vec![serde_json::to_string(&self.server.resources()).unwrap_or_default()],
            )),
            other => Some(WsMessage::new("daemon error", vec![format!("unknown event: {other}")])),
        };
        (Flow::Continue, reply)
    }

    fn on_auth(&mut self, token: &str) -> (Flow, Option<WsMessage>) {
        match self.app.jwt.websocket(token, self.server.uuid()) {
            Ok(c) if c.has_permission("websocket.connect") => {
                let first = self.claims.is_none();
                self.claims = Some(c);
                self.expiring_sent = false;
                if first {
                    let state = self.server.state();
                    let _ = self
                        .out_tx
                        .try_send(WsMessage::new("status", vec![state.as_str().into()]));
                }
                (Flow::Continue, Some(WsMessage::new("auth success", vec![])))
            }
            Ok(_) => (
                Flow::Close,
                Some(WsMessage::new("jwt error", vec!["missing websocket.connect permission".into()])),
            ),
            Err(e) => {
                let msg = match e {
                    JwtError::Invalid => "jwt: invalid or expired token",
                    JwtError::Denied => "jwt: token has been revoked",
                    JwtError::WrongServer => "jwt: token is for another server",
                };
                (Flow::Continue, Some(WsMessage::new("jwt error", vec![msg.into()])))
            }
        }
    }

    fn on_set_state(&self, arg: Option<&str>) -> Option<WsMessage> {
        let action = match arg {
            Some("start") => PowerAction::Start,
            Some("stop") => PowerAction::Stop,
            Some("restart") => PowerAction::Restart,
            Some("kill") => PowerAction::Kill,
            _ => return Some(WsMessage::new("daemon error", vec!["invalid power action".into()])),
        };
        let perm = format!("control.{}", arg.unwrap());
        if !self.can(&perm) {
            return Some(WsMessage::new("daemon error", vec![format!("missing permission {perm}")]));
        }
        let server = self.server.clone();
        let out = self.out_tx.clone();
        let detailed = self.can("admin.websocket.errors");
        let user = self.claims.as_ref().map(|c| c.user_uuid.clone());
        self.app.activity.record(
            server.uuid(),
            user.as_deref(),
            &format!("server:power.{}", arg.unwrap()),
            serde_json::json!({}),
            None,
        );
        tokio::spawn(async move {
            if let Err(e) = server.handle_power(action, None).await {
                let text = match &e {
                    crate::server::ServerError::Other(_) if !detailed => {
                        "an unexpected error occurred while processing the power action".to_string()
                    }
                    _ => e.to_string(),
                };
                let _ = out.send(WsMessage::new("daemon error", vec![text])).await;
            }
        });
        None
    }

    async fn on_command(&self, cmd: Option<String>) -> Option<WsMessage> {
        if !self.can("control.console") {
            return Some(WsMessage::new("daemon error", vec!["missing permission control.console".into()]));
        }
        let cmd = cmd?;
        if cmd.len() > 4096 {
            return Some(WsMessage::new("daemon error", vec!["command too long".into()]));
        }
        match self.server.send_command(&cmd).await {
            Ok(()) => {
                let user = self.claims.as_ref().map(|c| c.user_uuid.as_str());
                self.app.activity.record(
                    self.server.uuid(),
                    user,
                    "server:console.command",
                    serde_json::json!({ "command": cmd }),
                    None,
                );
                None
            }
            Err(e) => Some(WsMessage::new("daemon error", vec![e.to_string()])),
        }
    }
}
