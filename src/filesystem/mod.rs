pub mod archive;
pub mod ops;
pub mod pull;

pub use ops::{FileStat, FileStatExt};

use cap_std::fs::{Dir, File, OpenOptions};
use cap_std::ambient_authority;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error("file or directory not found")]
    NotFound,
    #[error("path is not permitted: {0}")]
    InvalidPath(String),
    #[error("this file cannot be modified: present in egg denylist")]
    Denied,
    #[error("not a directory")]
    NotDirectory,
    #[error("is a directory")]
    IsDirectory,
    #[error("not enough disk space available")]
    DiskSpace,
    #[error("file is too large")]
    TooLarge,
    #[error("already exists")]
    Exists,
    #[error("unsupported archive format")]
    UnsupportedArchive,
    #[error(transparent)]
    Io(io::Error),
}

impl From<io::Error> for FsError {
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::NotFound => FsError::NotFound,
            io::ErrorKind::AlreadyExists => FsError::Exists,
            io::ErrorKind::NotADirectory => FsError::NotDirectory,
            io::ErrorKind::IsADirectory => FsError::IsDirectory,
            io::ErrorKind::PermissionDenied
                if e.to_string().contains("outside of the filesystem") =>
            {
                FsError::InvalidPath("path escapes the server directory".into())
            }
            _ => FsError::Io(e),
        }
    }
}

impl From<&FsError> for crate::error::ApiError {
    fn from(e: &FsError) -> Self {
        use axum::http::StatusCode as S;
        let status = match e {
            FsError::NotFound => S::NOT_FOUND,
            FsError::InvalidPath(_)
            | FsError::NotDirectory
            | FsError::IsDirectory
            | FsError::UnsupportedArchive => S::BAD_REQUEST,
            FsError::Denied => S::FORBIDDEN,
            FsError::Exists => S::CONFLICT,
            FsError::DiskSpace => S::INSUFFICIENT_STORAGE,
            FsError::TooLarge => S::PAYLOAD_TOO_LARGE,
            FsError::Io(err) => {
                tracing::error!(error = %err, "filesystem io error");
                return crate::error::ApiError::internal("filesystem error");
            }
        };
        crate::error::ApiError::new(status, e.to_string())
    }
}

impl From<FsError> for crate::error::ApiError {
    fn from(e: FsError) -> Self {
        (&e).into()
    }
}

pub type FsResult<T> = Result<T, FsError>;

pub fn clean_path(input: &str) -> FsResult<PathBuf> {
    if input.contains('\0') {
        return Err(FsError::InvalidPath("path contains NUL byte".into()));
    }
    let mut out = PathBuf::new();
    for comp in Path::new(input).components() {
        match comp {
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
            Component::ParentDir => {
                if !out.pop() {
                    return Err(FsError::InvalidPath(
                        "path escapes the server directory".into(),
                    ));
                }
            }
            Component::Normal(p) => out.push(p),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    Ok(out)
}

pub fn join_clean(dir: &str, name: &str) -> FsResult<PathBuf> {
    clean_path(&format!("{}/{}", dir.trim_end_matches('/'), name))
}

pub struct Filesystem {
    root_path: PathBuf,
    uid: u32,
    gid: u32,
    disk_limit: AtomicU64,
    usage: AtomicU64,
    last_usage_check: RwLock<Option<Instant>>,
    usage_interval: Duration,
    denylist: RwLock<Arc<Gitignore>>,
}

impl Filesystem {
    pub fn new(
        root_path: PathBuf,
        disk_limit_bytes: u64,
        uid: u32,
        gid: u32,
        denylist: &[String],
        usage_interval: Duration,
    ) -> Self {
        let fs = Self {
            root_path,
            uid,
            gid,
            disk_limit: AtomicU64::new(disk_limit_bytes),
            usage: AtomicU64::new(0),
            last_usage_check: RwLock::new(None),
            usage_interval,
            denylist: RwLock::new(Arc::new(Gitignore::empty())),
        };
        fs.set_denylist(denylist);
        fs
    }

    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    pub fn uid(&self) -> u32 {
        self.uid
    }

    pub fn gid(&self) -> u32 {
        self.gid
    }

    pub fn set_disk_limit(&self, bytes: u64) {
        self.disk_limit.store(bytes, Ordering::Relaxed);
    }

    pub fn disk_limit(&self) -> u64 {
        self.disk_limit.load(Ordering::Relaxed)
    }

    pub fn set_denylist(&self, patterns: &[String]) {
        let mut b = GitignoreBuilder::new(&self.root_path);
        for p in patterns {
            if let Err(e) = b.add_line(None, p) {
                tracing::warn!(pattern = %p, error = %e, "invalid denylist pattern");
            }
        }
        let gi = b.build().unwrap_or_else(|_| Gitignore::empty());
        *self.denylist.write().unwrap() = Arc::new(gi);
    }

    pub fn is_denied(&self, path: &Path) -> bool {
        let gi = self.denylist.read().unwrap().clone();
        if gi.is_empty() {
            return false;
        }
        let abs = self.root_path.join(path);
        gi.matched_path_or_any_parents(&abs, false).is_ignore()
            || gi.matched_path_or_any_parents(&abs, true).is_ignore()
    }

    pub fn check_denied(&self, path: &Path) -> FsResult<()> {
        if self.is_denied(path) {
            Err(FsError::Denied)
        } else {
            Ok(())
        }
    }

    pub async fn ensure_root(&self) -> FsResult<()> {
        let root = self.root_path.clone();
        let (uid, gid) = (self.uid, self.gid);
        tokio::task::spawn_blocking(move || -> FsResult<()> {
            std::fs::create_dir_all(&root)?;
            chown_path(&root, uid, gid)?;
            Ok(())
        })
        .await
        .map_err(|e| FsError::Io(io::Error::other(e)))?
    }

    pub fn open_root(&self) -> FsResult<Dir> {
        Ok(Dir::open_ambient_dir(&self.root_path, ambient_authority())?)
    }

    pub fn open_read(&self, path: &Path) -> FsResult<File> {
        let root = self.open_root()?;
        let f = root.open(path)?;
        if f.metadata()?.is_dir() {
            return Err(FsError::IsDirectory);
        }
        Ok(f)
    }

    pub fn open_write(&self, path: &Path, truncate: bool) -> FsResult<File> {
        self.check_denied(path)?;
        let root = self.open_root()?;
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            self.create_dir_all_in(&root, parent)?;
        }
        let mut opts = OpenOptions::new();
        opts.write(true).create(true).truncate(truncate);
        let f = root.open_with(path, &opts)?;
        if f.metadata()?.is_dir() {
            return Err(FsError::IsDirectory);
        }
        self.chown_file(&f)?;
        Ok(f)
    }

    pub fn create_dir_all_in(&self, root: &Dir, path: &Path) -> FsResult<()> {
        let mut cur = PathBuf::new();
        for comp in path.components() {
            cur.push(comp);
            match root.create_dir(&cur) {
                Ok(()) => {
                    let d = root.open_dir(&cur)?;
                    chown_fd(&d, self.uid, self.gid)?;
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    if !root.metadata(&cur)?.is_dir() {
                        return Err(FsError::NotDirectory);
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    pub fn chown_file(&self, f: &File) -> FsResult<()> {
        chown_fd(f, self.uid, self.gid)
    }

    pub async fn read_to_string(self: &Arc<Self>, path: PathBuf) -> FsResult<String> {
        let this = self.clone();
        blocking(move || {
            use std::io::Read;
            let mut f = this.open_read(&path)?;
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)?;
            Ok(String::from_utf8_lossy(&buf).into_owned())
        })
        .await
    }

    pub async fn write_bytes(self: &Arc<Self>, path: PathBuf, data: Vec<u8>) -> FsResult<()> {
        self.ensure_space_for(data.len() as u64).await?;
        let this = self.clone();
        let len = data.len() as u64;
        blocking(move || {
            use std::io::Write;
            let mut f = this.open_write(&path, true)?;
            f.write_all(&data)?;
            Ok(())
        })
        .await?;
        self.add_usage(len as i64);
        Ok(())
    }

    pub async fn disk_usage(self: &Arc<Self>, force: bool) -> u64 {
        let fresh = self
            .last_usage_check
            .read()
            .unwrap()
            .is_some_and(|t| t.elapsed() < self.usage_interval);
        if fresh && !force {
            return self.usage.load(Ordering::Relaxed);
        }
        let root = self.root_path.clone();
        let size = tokio::task::spawn_blocking(move || dir_size(&root))
            .await
            .unwrap_or(0);
        self.usage.store(size, Ordering::Relaxed);
        *self.last_usage_check.write().unwrap() = Some(Instant::now());
        size
    }

    pub fn cached_usage(&self) -> u64 {
        self.usage.load(Ordering::Relaxed)
    }

    pub fn add_usage(&self, delta: i64) {
        if delta >= 0 {
            self.usage.fetch_add(delta as u64, Ordering::Relaxed);
        } else {
            let d = delta.unsigned_abs();
            let _ = self
                .usage
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |u| {
                    Some(u.saturating_sub(d))
                });
        }
    }

    pub async fn has_space_available(self: &Arc<Self>, allow_cached: bool) -> bool {
        let limit = self.disk_limit();
        if limit == 0 {
            return true;
        }
        self.disk_usage(!allow_cached).await < limit
    }

    pub async fn ensure_space_for(self: &Arc<Self>, extra: u64) -> FsResult<()> {
        let limit = self.disk_limit();
        if limit == 0 {
            return Ok(());
        }
        if self.disk_usage(false).await.saturating_add(extra) > limit {
            return Err(FsError::DiskSpace);
        }
        Ok(())
    }

    pub async fn truncate_root(self: &Arc<Self>) -> FsResult<()> {
        let this = self.clone();
        blocking(move || {
            let root = this.open_root()?;
            for entry in root.entries()? {
                let entry = entry?;
                let name = entry.file_name();
                if entry.file_type()?.is_dir() {
                    root.remove_dir_all(&name)?;
                } else {
                    root.remove_file(&name)?;
                }
            }
            Ok(())
        })
        .await?;
        self.usage.store(0, Ordering::Relaxed);
        Ok(())
    }

    pub async fn delete_root(&self) -> FsResult<()> {
        let root = self.root_path.clone();
        tokio::task::spawn_blocking(move || match std::fs::remove_dir_all(&root) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(FsError::from(e)),
            _ => Ok(()),
        })
        .await
        .map_err(|e| FsError::Io(io::Error::other(e)))?
    }
}

pub async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> FsResult<T> + Send + 'static,
) -> FsResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| FsError::Io(io::Error::other(e)))?
}

fn running_as_root() -> bool {
    rustix::process::geteuid().is_root()
}

pub fn chown_fd(fd: &impl std::os::fd::AsFd, uid: u32, gid: u32) -> FsResult<()> {
    if !running_as_root() {
        return Ok(());
    }
    rustix::fs::fchown(
        fd,
        Some(rustix::fs::Uid::from_raw(uid)),
        Some(rustix::fs::Gid::from_raw(gid)),
    )
    .map_err(|e| FsError::Io(e.into()))
}

fn chown_path(path: &Path, uid: u32, gid: u32) -> FsResult<()> {
    if !running_as_root() {
        return Ok(());
    }
    let f = std::fs::File::open(path)?;
    chown_fd(&f, uid, gid)
}

fn dir_size(root: &Path) -> u64 {
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_path_normalises() {
        assert_eq!(clean_path("").unwrap(), PathBuf::from("."));
        assert_eq!(clean_path("/").unwrap(), PathBuf::from("."));
        assert_eq!(clean_path("/a/b/../c").unwrap(), PathBuf::from("a/c"));
        assert_eq!(clean_path("./a//b/").unwrap(), PathBuf::from("a/b"));
        assert!(clean_path("../etc/passwd").is_err());
        assert!(clean_path("/a/../../etc").is_err());
        assert!(clean_path("a\0b").is_err());
    }

    fn fs_in(dir: &Path) -> Arc<Filesystem> {
        Arc::new(Filesystem::new(
            dir.to_path_buf(),
            0,
            1000,
            1000,
            &["*.secret".into(), "locked/".into()],
            Duration::from_secs(60),
        ))
    }

    #[tokio::test]
    async fn symlink_escape_is_blocked() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("host.txt"), "host secret").unwrap();
        let vol = tmp.path().join("vol");
        std::fs::create_dir(&vol).unwrap();
        std::os::unix::fs::symlink(outside.path(), vol.join("escape_dir")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", vol.join("passwd")).unwrap();
        std::os::unix::fs::symlink("../../", vol.join("up")).unwrap();

        let fs = fs_in(&vol);
        for p in ["escape_dir/host.txt", "passwd", "up/etc/passwd"] {
            let r = fs.read_to_string(PathBuf::from(p)).await;
            assert!(r.is_err(), "read through {p} must fail, got {r:?}");
        }
        let w = fs
            .write_bytes(PathBuf::from("escape_dir/pwned.txt"), b"x".to_vec())
            .await;
        assert!(w.is_err(), "write through symlink must fail");
        assert!(!outside.path().join("pwned.txt").exists());
    }

    #[tokio::test]
    async fn read_write_and_denylist() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = fs_in(tmp.path());
        fs.write_bytes(PathBuf::from("a/b/c.txt"), b"hello".to_vec())
            .await
            .unwrap();
        assert_eq!(
            fs.read_to_string(PathBuf::from("a/b/c.txt")).await.unwrap(),
            "hello"
        );
        assert!(matches!(
            fs.write_bytes(PathBuf::from("x.secret"), vec![]).await,
            Err(FsError::Denied)
        ));
        assert!(matches!(
            fs.write_bytes(PathBuf::from("locked/file"), vec![]).await,
            Err(FsError::Denied)
        ));
    }

    #[tokio::test]
    async fn disk_limit_enforced() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = fs_in(tmp.path());
        fs.set_disk_limit(10);
        fs.write_bytes(PathBuf::from("a"), vec![0; 8]).await.unwrap();
        assert!(matches!(
            fs.write_bytes(PathBuf::from("b"), vec![0; 8]).await,
            Err(FsError::DiskSpace)
        ));
    }
}
