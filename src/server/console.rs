use super::Server;
use super::events::{PowerState, ServerEvent};
use crate::environment::stats as st;
use bollard::container::{AttachContainerResults, LogOutput};
use futures_util::StreamExt;
use regex::Regex;
use std::sync::{Arc, LazyLock};
use std::sync::atomic::Ordering;
use std::time::Duration;

const MAX_LINE: usize = 8 * 1024;

static ANSI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*\x07").unwrap());

pub fn strip_ansi(s: &str) -> String {
    ANSI_RE.replace_all(s, "").into_owned()
}

#[derive(Default)]
pub struct LineSplitter {
    buf: Vec<u8>,
}

impl LineSplitter {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        for &b in chunk {
            if b == b'\n' || self.buf.len() >= MAX_LINE {
                out.push(self.take());
                if b == b'\n' {
                    continue;
                }
            }
            self.buf.push(b);
        }
        out
    }

    pub fn flush(&mut self) -> Option<String> {
        (!self.buf.is_empty()).then(|| self.take())
    }

    fn take(&mut self) -> String {
        let s = String::from_utf8_lossy(&self.buf)
            .trim_end_matches('\r')
            .to_string();
        self.buf.clear();
        s
    }
}

impl Server {
    pub(crate) async fn attach(self: &Arc<Self>) -> anyhow::Result<()> {
        let AttachContainerResults { mut output, input } = self.env.attach().await?;
        *self.stdin.lock().await = Some(input);

        let server = self.clone();
        let reader = tokio::spawn(async move {
            let mut split = LineSplitter::default();
            while let Some(item) = output.next().await {
                let bytes = match item {
                    Ok(LogOutput::StdOut { message })
                    | Ok(LogOutput::StdErr { message })
                    | Ok(LogOutput::Console { message }) => message,
                    Ok(_) => continue,
                    Err(e) => {
                        tracing::debug!(server = %server.uuid(), error = %e, "attach stream error");
                        break;
                    }
                };
                for line in split.push(&bytes) {
                    server.on_console_line(line);
                }
            }
            if let Some(line) = split.flush() {
                server.on_console_line(line);
            }
        });

        self.add_run_task(reader);
        Ok(())
    }

    pub(crate) fn watch_process(self: &Arc<Self>, run: u64) {
        let server = self.clone();
        tokio::spawn(async move {
            let exit = server.env.wait_for_exit().await;
            server.on_process_exit(run, exit).await;
        });
        let server = self.clone();
        self.add_run_task(tokio::spawn(async move { server.poll_stats().await }));
    }

    fn on_console_line(&self, line: String) {
        let starting = self.state() == PowerState::Starting;
        self.push_console_line(line.clone());
        if starting {
            let cfg = self.cfg.read().unwrap();
            let startup = &cfg.process_configuration.startup;
            let haystack = if startup.strip_ansi {
                strip_ansi(&line)
            } else {
                line.clone()
            };
            let done = startup
                .done
                .iter()
                .any(|d| !d.is_empty() && haystack.contains(d.as_str()));
            drop(cfg);
            if done {
                self.set_state(PowerState::Running);
            }
        }
    }

    async fn poll_stats(self: Arc<Self>) {
        let started = self.env.started_at().await;
        let mut stream = self.env.stats_stream();
        let mut last_disk_check = std::time::Instant::now() - Duration::from_secs(3600);
        while let Some(item) = stream.next().await {
            let Ok(s) = item else { break };
            if last_disk_check.elapsed() >= Duration::from_secs(10) {
                last_disk_check = std::time::Instant::now();
                let fs = self.fs.clone();
                tokio::spawn(async move {
                    fs.disk_usage(false).await;
                });
            }
            let (rx, tx) = st::network(&s);
            let uptime = started
                .map(|t| (chrono::Utc::now() - t).num_milliseconds().max(0) as u64)
                .unwrap_or(0);
            let usage = self.update_resources(|r| {
                r.memory_bytes = st::memory_usage(&s);
                r.memory_limit_bytes = st::memory_limit(&s);
                r.cpu_absolute = st::cpu_absolute(&s);
                r.network.rx_bytes = rx;
                r.network.tx_bytes = tx;
                r.uptime = uptime;
            });
            let mut usage = usage;
            usage.state = self.state();
            self.publish(ServerEvent::Stats(usage));
        }
    }

    pub(crate) async fn on_process_exit(
        self: &Arc<Self>,
        run: u64,
        exit: crate::environment::ExitInfo,
    ) {
        // A kill or a newer start already took over: ignore this run's exit so it cannot
        // flip the current run's state to offline or trigger a crash restart.
        if self.superseded(run) {
            tracing::debug!(server = %self.uuid(), "ignoring exit from superseded run");
            return;
        }
        self.abort_run_tasks();
        *self.stdin.lock().await = None;
        self.update_resources(|r| {
            r.memory_bytes = 0;
            r.cpu_absolute = 0.0;
            r.uptime = 0;
        });
        let expected = self.expect_exit.swap(false, Ordering::SeqCst);
        let was = self.state();
        self.set_state(PowerState::Offline);
        tracing::info!(
            server = %self.uuid(),
            exit_code = exit.exit_code,
            oom = exit.oom_killed,
            expected,
            "server process exited"
        );
        if !expected && was != PowerState::Offline {
            self.handle_crash(run, exit).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_lines_across_chunks() {
        let mut s = LineSplitter::default();
        assert_eq!(s.push(b"hel"), Vec::<String>::new());
        assert_eq!(s.push(b"lo\r\nwor"), vec!["hello".to_string()]);
        assert_eq!(s.push(b"ld\n\n"), vec!["world".to_string(), String::new()]);
        assert_eq!(s.flush(), None);
        s.push(b"tail");
        assert_eq!(s.flush(), Some("tail".into()));
    }

    #[test]
    fn strips_ansi() {
        assert_eq!(strip_ansi("\x1b[32mDone (1.2s)!\x1b[0m"), "Done (1.2s)!");
    }
}
