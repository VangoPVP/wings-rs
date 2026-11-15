use crate::remote::{ActivityEntry, RemoteClient};
use std::sync::Mutex;
use std::time::Duration;

const MAX_BUFFERED: usize = 5000;

#[derive(Default)]
pub struct ActivityLog {
    buf: Mutex<Vec<ActivityEntry>>,
}

impl ActivityLog {
    pub fn record(
        &self,
        server: &str,
        user: Option<&str>,
        event: &str,
        metadata: serde_json::Value,
        ip: Option<&str>,
    ) {
        let mut b = self.buf.lock().unwrap();
        if b.len() >= MAX_BUFFERED {
            b.remove(0);
        }
        b.push(ActivityEntry {
            user: user.map(String::from),
            server: server.to_string(),
            event: event.to_string(),
            metadata,
            ip: ip.map(String::from),
            timestamp: chrono::Utc::now(),
        });
    }

    fn take(&self) -> Vec<ActivityEntry> {
        std::mem::take(&mut *self.buf.lock().unwrap())
    }

    fn put_back(&self, mut entries: Vec<ActivityEntry>) {
        let mut b = self.buf.lock().unwrap();
        entries.append(&mut b);
        let excess = entries.len().saturating_sub(MAX_BUFFERED);
        entries.drain(..excess);
        *b = entries;
    }

    pub async fn run_flusher(&self, remote: &RemoteClient, interval: Duration) {
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            self.flush(remote).await;
        }
    }

    pub async fn flush(&self, remote: &RemoteClient) {
        let entries = self.take();
        if entries.is_empty() {
            return;
        }
        for chunk in entries.chunks(100) {
            if let Err(e) = remote.send_activity(chunk).await {
                tracing::debug!(error = %e, "failed to send activity, will retry");
                self.put_back(entries);
                return;
            }
        }
    }
}
