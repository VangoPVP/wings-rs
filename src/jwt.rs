use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum JwtError {
    #[error("jwt: token is invalid or expired")]
    Invalid,
    #[error("jwt: token has been revoked")]
    Denied,
    #[error("jwt: token is not valid for this server")]
    WrongServer,
}

pub use crate::protocol::jwt::*;

pub struct JwtValidator {
    key: DecodingKey,
    denied_jtis: Mutex<HashMap<String, Instant>>,
    used_ids: Mutex<HashMap<String, Instant>>,
}

const DENY_TTL: Duration = Duration::from_secs(60 * 60);

impl JwtValidator {
    pub fn new(secret: &str) -> anyhow::Result<Self> {
        Ok(Self {
            key: DecodingKey::from_secret(secret.as_bytes())?,
            denied_jtis: Mutex::new(HashMap::new()),
            used_ids: Mutex::new(HashMap::new()),
        })
    }

    pub fn decode<T: DeserializeOwned>(&self, token: &str) -> Result<T, JwtError> {
        let mut v = Validation::new(Algorithm::HS256);
        v.leeway = 5;
        v.validate_nbf = true;
        v.validate_aud = false;
        v.required_spec_claims = ["exp"].into_iter().map(String::from).collect();
        decode::<T>(token, &self.key, &v)
            .map(|d| d.claims)
            .map_err(|e| {
                tracing::debug!(error = %e, "jwt rejected");
                JwtError::Invalid
            })
    }

    pub fn websocket(&self, token: &str, server_uuid: &str) -> Result<WebsocketClaims, JwtError> {
        let claims: WebsocketClaims = self.decode(token)?;
        if claims.server_uuid != server_uuid {
            return Err(JwtError::WrongServer);
        }
        if let Some(jti) = &claims.base.jti
            && self.is_denied(jti)
        {
            return Err(JwtError::Denied);
        }
        Ok(claims)
    }

    pub fn deny(&self, jtis: impl IntoIterator<Item = String>) {
        let mut map = self.denied_jtis.lock().unwrap();
        let now = Instant::now();
        map.retain(|_, t| now.duration_since(*t) < DENY_TTL);
        for j in jtis {
            map.insert(j, now);
        }
    }

    pub fn is_denied(&self, jti: &str) -> bool {
        self.denied_jtis.lock().unwrap().contains_key(jti)
    }

    pub fn consume_once(&self, unique_id: &str, exp: u64) -> bool {
        let mut map = self.used_ids.lock().unwrap();
        let now = Instant::now();
        map.retain(|_, t| *t > now);
        if map.contains_key(unique_id) {
            return false;
        }
        let secs_left = exp.saturating_sub(chrono::Utc::now().timestamp() as u64) + 60;
        map.insert(unique_id.to_string(), now + Duration::from_secs(secs_left));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde::Serialize;

    fn mint(claims: &impl Serialize) -> String {
        encode(
            &Header::default(),
            claims,
            &EncodingKey::from_secret(b"secret").unwrap(),
        )
        .unwrap()
    }

    fn base(exp_offset: i64) -> BaseClaims {
        BaseClaims {
            jti: Some("j1".into()),
            exp: (chrono::Utc::now().timestamp() + exp_offset) as u64,
            iat: None,
            nbf: None,
        }
    }

    #[test]
    fn websocket_claims_roundtrip() {
        let v = JwtValidator::new("secret").unwrap();
        let t = mint(&WebsocketClaims {
            base: base(60),
            user_uuid: "u".into(),
            server_uuid: "s1".into(),
            permissions: vec!["control.console".into()],
        });
        let c = v.websocket(&t, "s1").unwrap();
        assert!(c.has_permission("control.console"));
        assert!(!c.has_permission("control.start"));
        assert!(matches!(v.websocket(&t, "s2"), Err(JwtError::WrongServer)));
        v.deny(["j1".to_string()]);
        assert!(matches!(v.websocket(&t, "s1"), Err(JwtError::Denied)));
    }

    #[test]
    fn expired_and_bad_signature_rejected() {
        let v = JwtValidator::new("secret").unwrap();
        let t = mint(&WebsocketClaims {
            base: base(-120),
            user_uuid: "u".into(),
            server_uuid: "s1".into(),
            permissions: vec![],
        });
        assert!(matches!(v.websocket(&t, "s1"), Err(JwtError::Invalid)));
        let other = JwtValidator::new("other").unwrap();
        let t = mint(&WebsocketClaims {
            base: base(60),
            user_uuid: "u".into(),
            server_uuid: "s1".into(),
            permissions: vec![],
        });
        assert!(matches!(other.websocket(&t, "s1"), Err(JwtError::Invalid)));
    }

    #[test]
    fn one_time_ids() {
        let v = JwtValidator::new("secret").unwrap();
        let exp = chrono::Utc::now().timestamp() as u64 + 60;
        assert!(v.consume_once("x", exp));
        assert!(!v.consume_once("x", exp));
    }
}
