pub mod parsers;

use crate::config::Config;
use crate::server::Server;
use crate::server::configuration::ServerConfigurationResponse;
use crate::filesystem::{FsError, clean_path};
use regex::Regex;
use serde_json::Value;
use std::sync::{Arc, LazyLock};

pub use parsers::Replacement;

static PLACEHOLDER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{\{\s*([A-Za-z0-9_.\-]+)\s*\}\}").unwrap());

pub struct PlaceholderContext {
    server: Value,
    config: Value,
    env: Value,
}

impl PlaceholderContext {
    pub fn new(cfg: &ServerConfigurationResponse, config: &Config) -> Self {
        let s = &cfg.settings;
        let mut build = serde_json::to_value(&s.build).unwrap_or_default();
        if let Value::Object(b) = &mut build {
            b.insert("default".into(), serde_json::to_value(&s.allocations.default).unwrap_or_default());
            b.insert("memory".into(), s.build.memory_limit.into());
            b.insert("env".into(), serde_json::to_value(&s.environment).unwrap_or_default());
            b.insert("mappings".into(), serde_json::to_value(&s.allocations.mappings).unwrap_or_default());
        }
        let server = serde_json::json!({
            "uuid": s.uuid,
            "build": build,
            "allocations": s.allocations,
        });
        let mut config = serde_json::to_value(config).unwrap_or_default();
        if let Value::Object(c) = &mut config {
            c.remove("token");
            c.remove("token_id");
        }
        Self {
            server,
            config,
            env: serde_json::to_value(&s.environment).unwrap_or_default(),
        }
    }

    fn lookup(&self, path: &str) -> Option<Value> {
        let (root, rest) = path.split_once('.')?;
        let base = match root {
            "server" => &self.server,
            "config" => &self.config,
            "env" => &self.env,
            _ => return None,
        };
        let mut cur = base;
        for part in rest.split('.') {
            cur = cur.get(part)?;
        }
        Some(cur.clone())
    }

    pub fn resolve(&self, input: &Value) -> Value {
        let Value::String(s) = input else {
            return input.clone();
        };
        if let Some(c) = PLACEHOLDER.captures(s)
            && c.get(0).unwrap().as_str() == s.trim()
            && let Some(v) = self.lookup(&c[1])
        {
            return v;
        }
        let out = PLACEHOLDER.replace_all(s, |c: &regex::Captures| match self.lookup(&c[1]) {
            Some(Value::String(v)) => v,
            Some(Value::Null) | None => c[0].to_string(),
            Some(v) => v.to_string(),
        });
        Value::String(out.into_owned())
    }
}

pub async fn apply_all(server: &Arc<Server>, cfg: &ServerConfigurationResponse) {
    let ctx = PlaceholderContext::new(cfg, &server.ctx.config);
    for file in &cfg.process_configuration.configs {
        if let Err(e) = apply_one(server, &ctx, file).await {
            tracing::warn!(server = %server.uuid(), file = %file.file, error = %e, "failed to update config file");
            server.daemon_message(format!("Failed to update configuration file {}: {e}", file.file));
        }
    }
}

async fn apply_one(
    server: &Arc<Server>,
    ctx: &PlaceholderContext,
    file: &crate::server::configuration::ConfigurationFile,
) -> anyhow::Result<()> {
    let path = clean_path(&file.file)?;
    let content = match server.fs.read_to_string(path.clone()).await {
        Ok(c) => c,
        Err(FsError::NotFound) => String::new(),
        Err(e) => return Err(e.into()),
    };
    let reps: Vec<Replacement> = file
        .replace
        .iter()
        .map(|r| Replacement {
            key: r.match_key.clone(),
            if_value: r.if_value.clone(),
            value: ctx.resolve(&r.replace_with),
        })
        .collect();
    let parser = file.parser.clone();
    let updated = tokio::task::spawn_blocking(move || parsers::apply(&parser, &content, &reps)).await??;
    server.fs.write_bytes(path, updated.into_bytes()).await?;
    Ok(())
}
