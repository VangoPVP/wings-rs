use crate::filesystem::archive::{extract_tar_gz, write_tar_gz};
use crate::remote::{BackupPart, BackupStatus};
use crate::server::Server;
use crate::server::events::ServerEvent;
use anyhow::{Context, Result, bail};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use sha1::{Digest, Sha1};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use crate::protocol::wings_api::BackupAdapter as Adapter;

pub fn valid_backup_uuid(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok() && !id.contains(['/', '\\', '.'])
}

pub fn local_path(server: &Server, backup: &str) -> PathBuf {
    server
        .ctx
        .config
        .system
        .backup_directory
        .join(format!("{backup}.tar.gz"))
}

pub(crate) struct HashingWriter<W: Write> {
    pub(crate) inner: W,
    pub(crate) hasher: Sha1,
    pub(crate) count: u64,
}

impl<W: Write> HashingWriter<W> {
    pub(crate) fn new(inner: W) -> Self {
        Self { inner, hasher: Sha1::new(), count: 0 }
    }

    pub(crate) fn hex_digest(&self) -> String {
        hex::encode(self.hasher.clone().finalize())
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.count += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn build_ignore(root: &Path, patterns: &str, pteroignore: Option<String>) -> Gitignore {
    let mut b = GitignoreBuilder::new(root);
    for line in patterns.lines().chain(pteroignore.as_deref().unwrap_or("").lines()) {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Err(e) = b.add_line(None, line) {
            tracing::debug!(pattern = line, error = %e, "invalid backup ignore pattern");
        }
    }
    b.build().unwrap_or_else(|_| Gitignore::empty())
}

async fn archive_to(server: &Arc<Server>, dest: PathBuf, ignore: String) -> Result<(String, u64)> {
    let pteroignore = server.fs.read_to_string(PathBuf::from(".pteroignore")).await.ok();
    let fs = server.fs.clone();
    let level = server.ctx.config.system.backups.compression_level;
    tokio::task::spawn_blocking(move || -> Result<(String, u64)> {
        let gi = build_ignore(fs.root_path(), &ignore, pteroignore);
        let tmp = dest.with_extension("partial");
        let file = std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        let writer = HashingWriter {
            inner: std::io::BufWriter::new(file),
            hasher: Sha1::new(),
            count: 0,
        };
        let result = write_tar_gz(&fs, Path::new("."), None, Some(&gi), writer, level);
        let mut w = match result {
            Ok(w) => w,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e.into());
            }
        };
        w.flush()?;
        let checksum = hex::encode(w.hasher.finalize());
        std::fs::rename(&tmp, &dest)?;
        Ok((checksum, w.count))
    })
    .await?
}

async fn upload_s3(server: &Arc<Server>, backup: &str, file: &Path, size: u64) -> Result<Vec<BackupPart>> {
    let urls = server.ctx.remote.get_backup_upload_urls(backup, size).await?;
    if urls.part_size == 0 || urls.parts.is_empty() {
        bail!("panel returned no upload parts");
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60 * 60))
        .build()?;
    let limit_bps = server.ctx.config.system.backups.write_limit * 1024 * 1024;
    let mut parts = Vec::new();
    let mut f = std::fs::File::open(file)?;
    for (i, url) in urls.parts.iter().enumerate() {
        let mut buf = Vec::with_capacity(urls.part_size.min(size) as usize);
        (&mut f).take(urls.part_size).read_to_end(&mut buf)?;
        if buf.is_empty() {
            break;
        }
        let started = Instant::now();
        let len = buf.len() as u64;
        let body = bytes::Bytes::from(buf);
        let mut attempt = 0;
        let etag = loop {
            attempt += 1;
            let res = client
                .put(url)
                .header(reqwest::header::CONTENT_LENGTH, len)
                .body(body.clone())
                .send()
                .await;
            match res {
                Ok(r) if r.status().is_success() => {
                    let etag = r
                        .headers()
                        .get(reqwest::header::ETAG)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    break etag;
                }
                Ok(r) if attempt >= 3 => bail!("part {} upload failed: {}", i + 1, r.status()),
                Err(e) if attempt >= 3 => bail!("part {} upload failed: {e}", i + 1),
                _ => tokio::time::sleep(Duration::from_secs(attempt)).await,
            }
        };
        parts.push(BackupPart {
            etag,
            part_number: (i + 1) as u32,
        });
        if limit_bps > 0 {
            let min = Duration::from_secs_f64(len as f64 / limit_bps as f64);
            if let Some(rest) = min.checked_sub(started.elapsed()) {
                tokio::time::sleep(rest).await;
            }
        }
    }
    Ok(parts)
}

pub async fn create(server: Arc<Server>, adapter: Adapter, backup: String, ignore: String) {
    server.daemon_message("Starting backup...");
    let result = async {
        let cfg = &server.ctx.config;
        match adapter {
            Adapter::Wings => {
                let dest = local_path(&server, &backup);
                let (checksum, size) = archive_to(&server, dest, ignore).await?;
                Ok::<_, anyhow::Error>((checksum, size, Vec::new()))
            }
            Adapter::S3 => {
                let tmp = cfg.system.tmp_directory.join(format!("{backup}.tar.gz"));
                let res = async {
                    let (checksum, size) = archive_to(&server, tmp.clone(), ignore).await?;
                    let parts = upload_s3(&server, &backup, &tmp, size).await?;
                    Ok::<_, anyhow::Error>((checksum, size, parts))
                }
                .await;
                let _ = tokio::fs::remove_file(&tmp).await;
                res
            }
        }
    }
    .await;

    let (status, ok) = match result {
        Ok((checksum, size, parts)) => (
            BackupStatus {
                checksum,
                checksum_type: "sha1".into(),
                size,
                successful: true,
                parts,
            },
            true,
        ),
        Err(e) => {
            tracing::error!(server = %server.uuid(), %backup, error = ?e, "backup failed");
            server.daemon_message(format!("Backup failed: {e}"));
            (
                BackupStatus {
                    checksum: String::new(),
                    checksum_type: "sha1".into(),
                    size: 0,
                    successful: false,
                    parts: vec![],
                },
                false,
            )
        }
    };
    if let Err(e) = server.ctx.remote.set_backup_status(&backup, &status).await {
        tracing::warn!(%backup, error = %e, "failed to report backup status");
    }
    server.publish(ServerEvent::BackupCompleted(serde_json::json!({
        "uuid": backup,
        "is_successful": ok,
        "checksum": status.checksum,
        "checksum_type": status.checksum_type,
        "file_size": status.size,
    })));
    if ok {
        server.daemon_message("Backup completed.");
    }
}

pub async fn restore(
    server: Arc<Server>,
    adapter: Adapter,
    backup: String,
    truncate: bool,
    download_url: Option<String>,
) {
    server.set_restoring(true);
    server.daemon_message("Restoring server from backup...");
    let result = async {
        if truncate {
            server.fs.truncate_root().await?;
        }
        let fs = server.fs.clone();
        match adapter {
            Adapter::Wings => {
                let path = local_path(&server, &backup);
                tokio::task::spawn_blocking(move || -> Result<u64> {
                    let f = std::fs::File::open(&path).context("backup file not found")?;
                    Ok(extract_tar_gz(&fs, std::io::BufReader::new(f), Path::new("."))?)
                })
                .await??;
            }
            Adapter::S3 => {
                let url = download_url.context("s3 restore requires a download_url")?;
                let res = reqwest::Client::new().get(&url).send().await?.error_for_status()?;
                use futures_util::TryStreamExt;
                let stream = res.bytes_stream().map_err(std::io::Error::other);
                let reader = tokio_util::io::StreamReader::new(stream);
                let handle = tokio::runtime::Handle::current();
                tokio::task::spawn_blocking(move || -> Result<u64> {
                    let sync = tokio_util::io::SyncIoBridge::new_with_handle(reader, handle);
                    Ok(extract_tar_gz(&fs, std::io::BufReader::new(sync), Path::new("."))?)
                })
                .await??;
            }
        }
        server.fs.disk_usage(true).await;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    server.set_restoring(false);
    let ok = match &result {
        Ok(()) => {
            server.daemon_message("Completed server restoration from backup.");
            true
        }
        Err(e) => {
            tracing::error!(server = %server.uuid(), %backup, error = ?e, "restore failed");
            server.daemon_message(format!("Backup restoration failed: {e}"));
            false
        }
    };
    if let Err(e) = server.ctx.remote.send_restoration_status(&backup, ok).await {
        tracing::warn!(%backup, error = %e, "failed to report restore status");
    }
    server.publish(ServerEvent::BackupRestoreCompleted);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_ids() {
        assert!(valid_backup_uuid("3f1b2c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d"));
        assert!(!valid_backup_uuid("../../etc/passwd"));
        assert!(!valid_backup_uuid("abc"));
    }

    #[test]
    fn hashing_writer() {
        let mut w = HashingWriter { inner: Vec::new(), hasher: Sha1::new(), count: 0 };
        w.write_all(b"abc").unwrap();
        assert_eq!(w.count, 3);
        assert_eq!(hex::encode(w.hasher.finalize()), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn ignore_merges_pteroignore() {
        let tmp = tempfile::tempdir().unwrap();
        let gi = build_ignore(tmp.path(), "logs/\n# comment\n", Some("*.tmp\n!keep.tmp".into()));
        assert!(gi.matched(tmp.path().join("logs"), true).is_ignore());
        assert!(gi.matched(tmp.path().join("a.tmp"), false).is_ignore());
        assert!(!gi.matched(tmp.path().join("keep.tmp"), false).is_ignore());
        assert!(!gi.matched(tmp.path().join("world"), true).is_ignore());
    }
}
