use crate::filesystem::{FsError, Filesystem};
use crate::server::Server;
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Metadata, MetadataExt, OpenOptions, Permissions, PermissionsExt};
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version,
};
use russh_sftp::server::StatusReply;
use std::collections::HashMap;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

const MAX_HANDLES: usize = 512;
const MAX_READ: u32 = 256 * 1024;
const READDIR_BATCH: usize = 100;

pub const PERM_READ: &str = "file.read";
pub const PERM_READ_CONTENT: &str = "file.read-content";
pub const PERM_CREATE: &str = "file.create";
pub const PERM_UPDATE: &str = "file.update";
pub const PERM_DELETE: &str = "file.delete";

pub fn sftp_rel(path: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for c in Path::new(path).components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::ParentDir => {
                out.pop();
            }
            _ => {}
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

pub fn sftp_abs(rel: &Path) -> String {
    if rel == Path::new(".") {
        "/".into()
    } else {
        format!("/{}", rel.display())
    }
}

fn status(code: StatusCode, msg: &str) -> StatusReply {
    StatusReply::new(code).with_message(msg)
}

fn denied() -> StatusReply {
    status(StatusCode::PermissionDenied, "permission denied")
}

fn map_fs(e: FsError) -> StatusReply {
    match e {
        FsError::NotFound => status(StatusCode::NoSuchFile, "no such file"),
        FsError::InvalidPath(_) | FsError::Denied => denied(),
        FsError::DiskSpace => status(StatusCode::Failure, "disk quota exceeded"),
        FsError::Exists => status(StatusCode::Failure, "file already exists"),
        FsError::IsDirectory => status(StatusCode::Failure, "is a directory"),
        FsError::NotDirectory => status(StatusCode::Failure, "not a directory"),
        other => {
            tracing::debug!(error = %other, "sftp io error");
            status(StatusCode::Failure, "operation failed")
        }
    }
}

fn map_io(e: io::Error) -> StatusReply {
    map_fs(FsError::from(e))
}

fn unix_secs(t: io::Result<cap_std::time::SystemTime>) -> Option<u32> {
    t.ok()
        .and_then(|t| t.into_std().duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs().min(u32::MAX as u64) as u32)
}

pub fn attrs_of(m: &Metadata) -> FileAttributes {
    FileAttributes {
        size: Some(m.len()),
        uid: Some(m.uid()),
        gid: Some(m.gid()),
        permissions: Some(m.mode()),
        atime: unix_secs(m.accessed()),
        mtime: unix_secs(m.modified()),
        ..Default::default()
    }
}

enum OpenHandle {
    File {
        file: Arc<std::fs::File>,
        path: PathBuf,
        writable: bool,
        append: bool,
        created: bool,
        wrote: bool,
    },
    Dir {
        entries: Vec<File>,
        pos: usize,
    },
}

#[derive(Clone)]
pub struct SessionInfo {
    pub user_uuid: String,
    pub ip: String,
    pub permissions: Vec<String>,
    pub read_only: bool,
}

pub struct SftpSession {
    fs: Arc<Filesystem>,
    server: Option<Arc<Server>>,
    info: SessionInfo,
    handles: HashMap<String, OpenHandle>,
    next_handle: u64,
}

impl SftpSession {
    pub fn new(fs: Arc<Filesystem>, server: Option<Arc<Server>>, info: SessionInfo) -> Self {
        Self {
            fs,
            server,
            info,
            handles: HashMap::new(),
            next_handle: 0,
        }
    }

    fn can(&self, perm: &str) -> bool {
        self.info.permissions.iter().any(|p| p == "*" || p == perm)
    }

    fn need(&self, perm: &str) -> Result<(), StatusReply> {
        if let Some(s) = &self.server
            && s.is_suspended()
        {
            return Err(denied());
        }
        let writes = matches!(perm, PERM_CREATE | PERM_UPDATE | PERM_DELETE);
        if (writes && self.info.read_only) || !self.can(perm) {
            return Err(denied());
        }
        Ok(())
    }

    fn not_denied(&self, rel: &Path) -> Result<(), StatusReply> {
        if self.fs.is_denied(rel) { Err(denied()) } else { Ok(()) }
    }

    fn record(&self, event: &str, files: &[&Path]) {
        if let Some(s) = &self.server {
            let files: Vec<String> = files.iter().map(|p| sftp_abs(p)).collect();
            s.ctx.activity.record(
                s.uuid(),
                Some(&self.info.user_uuid),
                &format!("server:sftp.{event}"),
                serde_json::json!({ "files": files }),
                Some(&self.info.ip),
            );
        }
    }

    fn add_handle(&mut self, h: OpenHandle) -> Result<String, StatusReply> {
        if self.handles.len() >= MAX_HANDLES {
            return Err(status(StatusCode::Failure, "too many open handles"));
        }
        self.next_handle += 1;
        let id = self.next_handle.to_string();
        self.handles.insert(id.clone(), h);
        Ok(id)
    }

    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Filesystem) -> Result<T, StatusReply> + Send + 'static,
    ) -> Result<T, StatusReply> {
        let fs = self.fs.clone();
        tokio::task::spawn_blocking(move || f(&fs))
            .await
            .map_err(|_| status(StatusCode::Failure, "internal error"))?
    }

    async fn stat_path(&self, path: String, follow: bool) -> Result<FileAttributes, StatusReply> {
        self.need(PERM_READ)?;
        let rel = sftp_rel(&path);
        self.blocking(move |fs| {
            let root = fs.open_root().map_err(map_fs)?;
            let m = if follow {
                root.metadata(&rel)
            } else {
                root.symlink_metadata(&rel)
            }
            .map_err(map_io)?;
            Ok(attrs_of(&m))
        })
        .await
    }

    async fn apply_attrs(&self, rel: PathBuf, attrs: FileAttributes) -> Result<(), StatusReply> {
        self.blocking(move |fs| {
            let root = fs.open_root().map_err(map_fs)?;
            let meta = root.symlink_metadata(&rel).map_err(map_io)?;
            if meta.is_symlink() {
                return Err(denied());
            }
            if let Some(mode) = attrs.permissions {
                root.set_permissions(&rel, Permissions::from_mode(mode & 0o777))
                    .map_err(map_io)?;
            }
            if attrs.size.is_some() || attrs.mtime.is_some() || attrs.atime.is_some() {
                let mut opts = OpenOptions::new();
                opts.write(attrs.size.is_some())
                    .read(attrs.size.is_none())
                    .follow(FollowSymlinks::No);
                let f = root.open_with(&rel, &opts).map_err(map_io)?.into_std();
                if let Some(size) = attrs.size {
                    f.set_len(size).map_err(map_io)?;
                }
                let to_time =
                    |s: u32| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s as u64);
                let mut times = std::fs::FileTimes::new();
                if let Some(m) = attrs.mtime {
                    times = times.set_modified(to_time(m));
                }
                if let Some(a) = attrs.atime {
                    times = times.set_accessed(to_time(a));
                }
                let _ = f.set_times(times);
            }
            Ok(())
        })
        .await
    }
}

fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".into(),
        language_tag: "en-US".into(),
    }
}

impl russh_sftp::server::Handler for SftpSession {
    type Error = StatusReply;

    fn unimplemented(&self) -> Self::Error {
        StatusReply::new(StatusCode::OpUnsupported)
    }

    async fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, Self::Error> {
        Ok(Version::new())
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let rel = sftp_rel(&filename);
        if rel == Path::new(".") {
            return Err(status(StatusCode::Failure, "is a directory"));
        }
        self.not_denied(&rel)?;
        let read = pflags.contains(OpenFlags::READ);
        let writable = pflags.intersects(
            OpenFlags::WRITE | OpenFlags::APPEND | OpenFlags::CREATE | OpenFlags::TRUNCATE,
        );
        let exists = {
            let r = rel.clone();
            self.blocking(move |fs| {
                Ok(fs.open_root().map_err(map_fs)?.symlink_metadata(&r).is_ok())
            })
            .await?
        };
        if read {
            self.need(PERM_READ_CONTENT)?;
        }
        if writable {
            if exists {
                self.need(PERM_UPDATE)?;
            } else {
                self.need(PERM_CREATE)?;
            }
            if !self.fs.has_space_available(true).await {
                return Err(map_fs(FsError::DiskSpace));
            }
        }
        if !exists && !pflags.contains(OpenFlags::CREATE) {
            return Err(status(StatusCode::NoSuchFile, "no such file"));
        }
        let create = pflags.contains(OpenFlags::CREATE);
        let excl = pflags.contains(OpenFlags::EXCLUDE);
        let trunc = pflags.contains(OpenFlags::TRUNCATE);
        let r = rel.clone();
        let file = self
            .blocking(move |fs| {
                let root = fs.open_root().map_err(map_fs)?;
                let mut opts = OpenOptions::new();
                opts.read(read || !writable)
                    .write(writable)
                    .create(create && !excl)
                    .create_new(create && excl)
                    .truncate(trunc);
                let f = root.open_with(&r, &opts).map_err(map_io)?;
                if f.metadata().map_err(map_io)?.is_dir() {
                    return Err(status(StatusCode::Failure, "is a directory"));
                }
                if writable && !exists {
                    fs.chown_file(&f).map_err(map_fs)?;
                }
                Ok(f.into_std())
            })
            .await?;
        let handle = self.add_handle(OpenHandle::File {
            file: Arc::new(file),
            path: rel,
            writable,
            append: pflags.contains(OpenFlags::APPEND),
            created: !exists,
            wrote: false,
        })?;
        Ok(Handle { id, handle })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        match self.handles.remove(&handle) {
            Some(OpenHandle::File { path, created, wrote, .. }) => {
                if created {
                    self.record("create", &[&path]);
                } else if wrote {
                    self.record("write", &[&path]);
                }
                Ok(ok(id))
            }
            Some(OpenHandle::Dir { .. }) => Ok(ok(id)),
            None => Err(status(StatusCode::Failure, "invalid handle")),
        }
    }

    async fn read(&mut self, id: u32, handle: String, offset: u64, len: u32) -> Result<Data, Self::Error> {
        if let Some(s) = &self.server
            && s.is_suspended()
        {
            return Err(denied());
        }
        let Some(OpenHandle::File { file, .. }) = self.handles.get(&handle) else {
            return Err(status(StatusCode::Failure, "invalid handle"));
        };
        let file = file.clone();
        let len = len.min(MAX_READ) as usize;
        let data = tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; len];
            let n = file.read_at(&mut buf, offset)?;
            buf.truncate(n);
            Ok::<_, io::Error>(buf)
        })
        .await
        .map_err(|_| status(StatusCode::Failure, "internal error"))?
        .map_err(map_io)?;
        if data.is_empty() && len > 0 {
            return Err(StatusReply::new(StatusCode::Eof));
        }
        Ok(Data { id, data })
    }

    async fn write(&mut self, id: u32, handle: String, offset: u64, data: Vec<u8>) -> Result<Status, Self::Error> {
        if let Some(s) = &self.server
            && s.is_suspended()
        {
            return Err(denied());
        }
        let (file, append) = match self.handles.get(&handle) {
            Some(OpenHandle::File { file, writable: true, append, .. }) => (file.clone(), *append),
            Some(OpenHandle::File { .. }) => return Err(denied()),
            _ => return Err(status(StatusCode::Failure, "invalid handle")),
        };
        let len = data.len() as u64;
        self.fs.ensure_space_for(len).await.map_err(map_fs)?;
        tokio::task::spawn_blocking(move || {
            let at = if append { file.metadata()?.len() } else { offset };
            file.write_all_at(&data, at)
        })
        .await
        .map_err(|_| status(StatusCode::Failure, "internal error"))?
        .map_err(map_io)?;
        self.fs.add_usage(len as i64);
        if let Some(OpenHandle::File { wrote, .. }) = self.handles.get_mut(&handle) {
            *wrote = true;
        }
        Ok(ok(id))
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        Ok(Attrs { id, attrs: self.stat_path(path, false).await? })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        Ok(Attrs { id, attrs: self.stat_path(path, true).await? })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let Some(OpenHandle::File { file, .. }) = self.handles.get(&handle) else {
            return Err(status(StatusCode::Failure, "invalid handle"));
        };
        let f = file.clone();
        let m = tokio::task::spawn_blocking(move || f.metadata())
            .await
            .map_err(|_| status(StatusCode::Failure, "internal error"))?
            .map_err(map_io)?;
        let attrs = FileAttributes {
            size: Some(m.len()),
            uid: Some(std::os::unix::fs::MetadataExt::uid(&m)),
            gid: Some(std::os::unix::fs::MetadataExt::gid(&m)),
            permissions: Some(std::os::unix::fs::MetadataExt::mode(&m)),
            mtime: Some(std::os::unix::fs::MetadataExt::mtime(&m).max(0) as u32),
            atime: Some(std::os::unix::fs::MetadataExt::atime(&m).max(0) as u32),
            ..Default::default()
        };
        Ok(Attrs { id, attrs })
    }

    async fn setstat(&mut self, id: u32, path: String, attrs: FileAttributes) -> Result<Status, Self::Error> {
        self.need(PERM_UPDATE)?;
        let rel = sftp_rel(&path);
        self.not_denied(&rel)?;
        self.apply_attrs(rel, attrs).await?;
        Ok(ok(id))
    }

    async fn fsetstat(&mut self, id: u32, handle: String, attrs: FileAttributes) -> Result<Status, Self::Error> {
        self.need(PERM_UPDATE)?;
        let Some(OpenHandle::File { path, .. }) = self.handles.get(&handle) else {
            return Err(status(StatusCode::Failure, "invalid handle"));
        };
        let rel = path.clone();
        self.apply_attrs(rel, attrs).await?;
        Ok(ok(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        self.need(PERM_READ)?;
        let rel = sftp_rel(&path);
        let entries = self
            .blocking(move |fs| {
                let root = fs.open_root().map_err(map_fs)?;
                let dir = if rel == Path::new(".") {
                    root
                } else {
                    root.open_dir(&rel).map_err(map_io)?
                };
                let mut out = Vec::new();
                for e in dir.entries().map_err(map_io)?.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if let Ok(m) = e.metadata() {
                        out.push(File::new(name, attrs_of(&m)));
                    }
                }
                out.sort_by(|a, b| a.filename.cmp(&b.filename));
                Ok(out)
            })
            .await?;
        let handle = self.add_handle(OpenHandle::Dir { entries, pos: 0 })?;
        Ok(Handle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        let Some(OpenHandle::Dir { entries, pos }) = self.handles.get_mut(&handle) else {
            return Err(status(StatusCode::Failure, "invalid handle"));
        };
        if *pos >= entries.len() {
            return Err(StatusReply::new(StatusCode::Eof));
        }
        let end = (*pos + READDIR_BATCH).min(entries.len());
        let files = entries[*pos..end].to_vec();
        *pos = end;
        Ok(Name { id, files })
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        self.need(PERM_DELETE)?;
        let rel = sftp_rel(&filename);
        self.not_denied(&rel)?;
        let r = rel.clone();
        let freed = self
            .blocking(move |fs| {
                let root = fs.open_root().map_err(map_fs)?;
                let m = root.symlink_metadata(&r).map_err(map_io)?;
                if m.is_dir() {
                    return Err(status(StatusCode::Failure, "is a directory"));
                }
                root.remove_file(&r).map_err(map_io)?;
                Ok(if m.is_file() { m.len() } else { 0 })
            })
            .await?;
        self.fs.add_usage(-(freed as i64));
        self.record("delete", &[&rel]);
        Ok(ok(id))
    }

    async fn mkdir(&mut self, id: u32, path: String, _attrs: FileAttributes) -> Result<Status, Self::Error> {
        self.need(PERM_CREATE)?;
        let rel = sftp_rel(&path);
        if rel == Path::new(".") {
            return Err(status(StatusCode::Failure, "file already exists"));
        }
        self.not_denied(&rel)?;
        let r = rel.clone();
        self.blocking(move |fs| {
            let root = fs.open_root().map_err(map_fs)?;
            root.create_dir(&r).map_err(map_io)?;
            let d = root.open_dir(&r).map_err(map_io)?;
            crate::filesystem::chown_fd(&d, fs.uid(), fs.gid()).map_err(map_fs)?;
            Ok(())
        })
        .await?;
        self.record("create-directory", &[&rel]);
        Ok(ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        self.need(PERM_DELETE)?;
        let rel = sftp_rel(&path);
        if rel == Path::new(".") {
            return Err(denied());
        }
        self.not_denied(&rel)?;
        let r = rel.clone();
        self.blocking(move |fs| fs.open_root().map_err(map_fs)?.remove_dir(&r).map_err(map_io))
            .await?;
        self.record("delete", &[&rel]);
        Ok(ok(id))
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        self.need(PERM_READ)?;
        let abs = sftp_abs(&sftp_rel(&path));
        Ok(Name { id, files: vec![File::dummy(abs)] })
    }

    async fn rename(&mut self, id: u32, oldpath: String, newpath: String) -> Result<Status, Self::Error> {
        self.need(PERM_UPDATE)?;
        let (from, to) = (sftp_rel(&oldpath), sftp_rel(&newpath));
        if from == Path::new(".") || to == Path::new(".") {
            return Err(denied());
        }
        self.not_denied(&from)?;
        self.not_denied(&to)?;
        let (f, t) = (from.clone(), to.clone());
        self.blocking(move |fs| {
            let root = fs.open_root().map_err(map_fs)?;
            if root.symlink_metadata(&t).is_ok() {
                return Err(status(StatusCode::Failure, "target already exists"));
            }
            root.rename(&f, &root, &t).map_err(map_io)
        })
        .await?;
        self.record("rename", &[&from, &to]);
        Ok(ok(id))
    }

    async fn readlink(&mut self, _id: u32, _path: String) -> Result<Name, Self::Error> {
        Err(StatusReply::new(StatusCode::OpUnsupported))
    }

    async fn symlink(&mut self, _id: u32, _linkpath: String, _targetpath: String) -> Result<Status, Self::Error> {
        Err(denied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh_sftp::server::Handler;
    use std::time::Duration;

    fn session(dir: &Path, perms: &[&str]) -> SftpSession {
        let fs = Arc::new(Filesystem::new(
            dir.to_path_buf(),
            0,
            1000,
            1000,
            &["*.secret".into()],
            Duration::from_secs(60),
        ));
        SftpSession::new(
            fs,
            None,
            SessionInfo {
                user_uuid: "u".into(),
                ip: "127.0.0.1".into(),
                permissions: perms.iter().map(|s| s.to_string()).collect(),
                read_only: false,
            },
        )
    }

    fn flags(f: OpenFlags) -> OpenFlags {
        f
    }

    #[test]
    fn path_mapping_clamps() {
        assert_eq!(sftp_rel("/"), PathBuf::from("."));
        assert_eq!(sftp_rel("/../../etc/passwd"), PathBuf::from("etc/passwd"));
        assert_eq!(sftp_rel("a/./b/../c"), PathBuf::from("a/c"));
        assert_eq!(sftp_abs(Path::new(".")), "/");
        assert_eq!(sftp_abs(Path::new("a/b")), "/a/b");
    }

    #[tokio::test]
    async fn write_read_rename_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = session(tmp.path(), &["*"]);
        let h = s
            .open(1, "/dir/../hello.txt".into(), flags(OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE), FileAttributes::default())
            .await
            .unwrap()
            .handle;
        s.write(2, h.clone(), 0, b"hello sftp".to_vec()).await.unwrap();
        s.close(3, h).await.unwrap();
        assert_eq!(std::fs::read_to_string(tmp.path().join("hello.txt")).unwrap(), "hello sftp");

        let h = s.open(4, "/hello.txt".into(), OpenFlags::READ, FileAttributes::default()).await.unwrap().handle;
        let d = s.read(5, h.clone(), 6, 100).await.unwrap();
        assert_eq!(d.data, b"sftp");
        assert!(s.read(6, h.clone(), 100, 10).await.is_err(), "EOF");
        s.close(7, h).await.unwrap();

        s.mkdir(8, "/sub".into(), FileAttributes::default()).await.unwrap();
        s.rename(9, "/hello.txt".into(), "/sub/moved.txt".into()).await.unwrap();
        let h = s.opendir(10, "/sub".into()).await.unwrap().handle;
        let names: Vec<_> = s.readdir(11, h.clone()).await.unwrap().files.into_iter().map(|f| f.filename).collect();
        assert_eq!(names, vec!["moved.txt"]);
        assert!(s.readdir(12, h).await.is_err(), "EOF after listing");
        s.remove(13, "/sub/moved.txt".into()).await.unwrap();
        s.rmdir(14, "/sub".into()).await.unwrap();
        assert!(!tmp.path().join("sub").exists());
    }

    #[tokio::test]
    async fn permissions_enforced() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "x").unwrap();
        let mut s = session(tmp.path(), &[PERM_READ]);
        assert!(s.stat(1, "/a.txt".into()).await.is_ok());
        let e = s.open(2, "/a.txt".into(), OpenFlags::READ, FileAttributes::default()).await.unwrap_err();
        assert_eq!(e.status_code, StatusCode::PermissionDenied);
        let e = s.open(3, "/new.txt".into(), OpenFlags::WRITE | OpenFlags::CREATE, FileAttributes::default()).await.unwrap_err();
        assert_eq!(e.status_code, StatusCode::PermissionDenied);
        assert!(s.remove(4, "/a.txt".into()).await.is_err());
        assert!(s.rename(5, "/a.txt".into(), "/b.txt".into()).await.is_err());

        let mut s = session(tmp.path(), &[PERM_CREATE]);
        assert!(s.open(6, "/fresh.txt".into(), OpenFlags::WRITE | OpenFlags::CREATE, FileAttributes::default()).await.is_ok());
        assert!(s.open(7, "/a.txt".into(), OpenFlags::WRITE | OpenFlags::TRUNCATE, FileAttributes::default()).await.is_err());
    }

    #[tokio::test]
    async fn read_only_and_denylist() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = session(tmp.path(), &["*"]);
        s.info.read_only = true;
        assert!(s.mkdir(1, "/x".into(), FileAttributes::default()).await.is_err());
        s.info.read_only = false;
        let e = s.open(2, "/key.secret".into(), OpenFlags::WRITE | OpenFlags::CREATE, FileAttributes::default()).await.unwrap_err();
        assert_eq!(e.status_code, StatusCode::PermissionDenied);
        assert!(s.symlink(3, "/l".into(), "/etc/passwd".into()).await.is_err());
    }

    #[tokio::test]
    async fn symlink_escape_blocked() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("host.txt"), "secret").unwrap();
        let vol = tmp.path().join("vol");
        std::fs::create_dir(&vol).unwrap();
        std::os::unix::fs::symlink(outside.path(), vol.join("out")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", vol.join("pw")).unwrap();
        let mut s = session(&vol, &["*"]);
        assert!(s.open(1, "/out/host.txt".into(), OpenFlags::READ, FileAttributes::default()).await.is_err());
        assert!(s.open(2, "/pw".into(), OpenFlags::READ, FileAttributes::default()).await.is_err());
        assert!(s.open(3, "/out/new.txt".into(), OpenFlags::WRITE | OpenFlags::CREATE, FileAttributes::default()).await.is_err());
        assert!(s.opendir(4, "/out".into()).await.is_err());
        assert!(s.stat(5, "/pw".into()).await.is_err());
        assert!(s.lstat(6, "/pw".into()).await.is_ok());
        assert!(!outside.path().join("new.txt").exists());
    }

    #[tokio::test]
    async fn quota_enforced() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = session(tmp.path(), &["*"]);
        s.fs.set_disk_limit(100);
        let h = s.open(1, "/f".into(), OpenFlags::WRITE | OpenFlags::CREATE, FileAttributes::default()).await.unwrap().handle;
        s.write(2, h.clone(), 0, vec![0; 80]).await.unwrap();
        let e = s.write(3, h, 80, vec![0; 80]).await.unwrap_err();
        assert_eq!(e.status_code, StatusCode::Failure);
    }
}
