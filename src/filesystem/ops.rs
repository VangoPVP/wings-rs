use super::{FsError, FsResult, Filesystem, blocking};
use cap_fs_ext::DirExt;
use cap_std::fs::{Dir, Metadata, MetadataExt, Permissions, PermissionsExt};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

pub use crate::protocol::files::FileStat;

fn rfc3339(t: io::Result<cap_std::time::SystemTime>) -> String {
    t.map(|t| chrono::DateTime::<chrono::Utc>::from(t.into_std()).to_rfc3339())
        .unwrap_or_default()
}

fn mode_string(m: &Metadata) -> String {
    let mode = m.mode();
    let kind = if m.is_symlink() {
        'l'
    } else if m.is_dir() {
        'd'
    } else {
        '-'
    };
    let mut s = String::with_capacity(10);
    s.push(kind);
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 0o7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        s.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    s
}

pub trait FileStatExt {
    fn from_metadata(name: &str, m: &Metadata) -> Self;
}

impl FileStatExt for FileStat {
    fn from_metadata(name: &str, m: &Metadata) -> Self {
        let mime = if m.is_dir() {
            "inode/directory".to_string()
        } else if m.is_symlink() {
            "inode/symlink".to_string()
        } else {
            mime_guess::from_path(name)
                .first_or_octet_stream()
                .essence_str()
                .to_string()
        };
        Self {
            name: name.to_string(),
            created: rfc3339(m.created()),
            modified: rfc3339(m.modified()),
            mode: mode_string(m),
            mode_bits: format!("{:o}", m.mode() & 0o777),
            size: m.len(),
            directory: m.is_dir(),
            file: m.is_file(),
            symlink: m.is_symlink(),
            mime,
        }
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/".into())
}

pub(crate) fn tree_size(root: &Dir, path: &Path) -> u64 {
    let Ok(m) = root.symlink_metadata(path) else {
        return 0;
    };
    if !m.is_dir() {
        return if m.is_file() { m.len() } else { 0 };
    }
    let Ok(dir) = root.open_dir_nofollow(path) else {
        return 0;
    };
    let Ok(entries) = dir.entries() else { return 0 };
    entries
        .flatten()
        .map(|e| tree_size(&dir, Path::new(&e.file_name())))
        .sum()
}

fn ensure_not_root(path: &Path) -> FsResult<()> {
    if path == Path::new(".") || path.as_os_str().is_empty() {
        return Err(FsError::InvalidPath("cannot modify the server root directory".into()));
    }
    Ok(())
}

impl Filesystem {
    pub async fn list_directory(self: &Arc<Self>, path: PathBuf) -> FsResult<Vec<FileStat>> {
        let this = self.clone();
        blocking(move || {
            let root = this.open_root()?;
            if !root.metadata(&path)?.is_dir() {
                return Err(FsError::NotDirectory);
            }
            let dir = root.open_dir(&path)?;
            let mut out = Vec::new();
            for entry in dir.entries()? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                let meta = entry.metadata()?;
                out.push(FileStat::from_metadata(&name, &meta));
            }
            out.sort_by(|a, b| b.directory.cmp(&a.directory).then_with(|| a.name.cmp(&b.name)));
            Ok(out)
        })
        .await
    }

    pub async fn stat(self: &Arc<Self>, path: PathBuf) -> FsResult<FileStat> {
        let this = self.clone();
        blocking(move || {
            let root = this.open_root()?;
            let meta = root.symlink_metadata(&path)?;
            Ok(FileStat::from_metadata(&file_name(&path), &meta))
        })
        .await
    }

    pub async fn rename(self: &Arc<Self>, from: PathBuf, to: PathBuf) -> FsResult<()> {
        ensure_not_root(&from)?;
        ensure_not_root(&to)?;
        self.check_denied(&from)?;
        self.check_denied(&to)?;
        let this = self.clone();
        blocking(move || {
            let root = this.open_root()?;
            root.symlink_metadata(&from)?;
            if root.symlink_metadata(&to).is_ok() {
                return Err(FsError::Exists);
            }
            if let Some(parent) = to.parent()
                && !parent.as_os_str().is_empty()
            {
                this.create_dir_all_in(&root, parent)?;
            }
            root.rename(&from, &root, &to)?;
            Ok(())
        })
        .await
    }

    pub async fn copy(self: &Arc<Self>, path: PathBuf) -> FsResult<FileStat> {
        ensure_not_root(&path)?;
        self.check_denied(&path)?;
        let size = {
            let this = self.clone();
            let p = path.clone();
            blocking(move || {
                let m = this.open_root()?.symlink_metadata(&p)?;
                if !m.is_file() {
                    return Err(FsError::IsDirectory);
                }
                Ok(m.len())
            })
            .await?
        };
        self.ensure_space_for(size).await?;
        let this = self.clone();
        let stat = blocking(move || {
            let root = this.open_root()?;
            let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
            let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let ext = path
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy()))
                .unwrap_or_default();
            let mut n = 1;
            let target = loop {
                let name = if n == 1 {
                    format!("{stem} copy{ext}")
                } else {
                    format!("{stem} copy {n}{ext}")
                };
                let candidate = parent.join(&name);
                if root.symlink_metadata(&candidate).is_err() {
                    break candidate;
                }
                n += 1;
                if n > 1000 {
                    return Err(FsError::Exists);
                }
            };
            let mut src = this.open_read(&path)?;
            let mut opts = cap_std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            let mut dst = root.open_with(&target, &opts)?;
            this.chown_file(&dst)?;
            io::copy(&mut src, &mut dst)?;
            let meta = root.symlink_metadata(&target)?;
            Ok(FileStat::from_metadata(&file_name(&target), &meta))
        })
        .await?;
        self.add_usage(size as i64);
        Ok(stat)
    }

    pub async fn delete(self: &Arc<Self>, paths: Vec<PathBuf>) -> FsResult<usize> {
        for p in &paths {
            ensure_not_root(p)?;
            self.check_denied(p)?;
        }
        let this = self.clone();
        let (count, freed) = blocking(move || {
            let root = this.open_root()?;
            let (mut count, mut freed) = (0usize, 0u64);
            for p in paths {
                let meta = match root.symlink_metadata(&p) {
                    Ok(m) => m,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e.into()),
                };
                let size = tree_size(&root, &p);
                if meta.is_dir() {
                    root.remove_dir_all(&p)?;
                } else {
                    root.remove_file(&p)?;
                }
                count += 1;
                freed += size;
            }
            Ok((count, freed))
        })
        .await?;
        self.add_usage(-(freed as i64));
        Ok(count)
    }

    pub async fn create_directory(self: &Arc<Self>, path: PathBuf) -> FsResult<()> {
        ensure_not_root(&path)?;
        self.check_denied(&path)?;
        let this = self.clone();
        blocking(move || {
            let root = this.open_root()?;
            this.create_dir_all_in(&root, &path)
        })
        .await
    }

    pub async fn chmod(self: &Arc<Self>, path: PathBuf, mode: u32) -> FsResult<()> {
        ensure_not_root(&path)?;
        self.check_denied(&path)?;
        let this = self.clone();
        blocking(move || {
            let root = this.open_root()?;
            if root.symlink_metadata(&path)?.is_symlink() {
                return Err(FsError::InvalidPath("cannot chmod a symlink".into()));
            }
            root.set_permissions(&path, Permissions::from_mode(mode & 0o777))?;
            Ok(())
        })
        .await
    }

    pub async fn open_async_read(self: &Arc<Self>, path: PathBuf) -> FsResult<(tokio::fs::File, u64)> {
        let this = self.clone();
        blocking(move || {
            let f = this.open_read(&path)?;
            let len = f.metadata()?.len();
            Ok((tokio::fs::File::from_std(f.into_std()), len))
        })
        .await
    }

    pub async fn write_stream<R: AsyncRead + Unpin>(
        self: &Arc<Self>,
        path: PathBuf,
        mut reader: R,
        max_bytes: Option<u64>,
    ) -> FsResult<u64> {
        ensure_not_root(&path)?;
        if !self.has_space_available(true).await {
            return Err(FsError::DiskSpace);
        }
        let file = {
            let this = self.clone();
            let p = path.clone();
            blocking(move || this.open_write(&p, true)).await?
        };
        let mut file = tokio::fs::File::from_std(file.into_std());
        let limit = self.disk_limit();
        let base_usage = self.cached_usage();
        let mut written: u64 = 0;
        let mut buf = vec![0u8; 64 * 1024];
        let result: FsResult<()> = async {
            loop {
                let n = reader.read(&mut buf).await.map_err(FsError::Io)?;
                if n == 0 {
                    break;
                }
                written += n as u64;
                if max_bytes.is_some_and(|m| written > m) {
                    return Err(FsError::TooLarge);
                }
                if limit > 0 && base_usage + written > limit {
                    return Err(FsError::DiskSpace);
                }
                file.write_all(&buf[..n]).await?;
            }
            file.flush().await?;
            Ok(())
        }
        .await;
        drop(file);
        if let Err(e) = result {
            let this = self.clone();
            let _ = blocking(move || Ok(this.open_root()?.remove_file(&path)?)).await;
            return Err(e);
        }
        self.add_usage(written as i64);
        Ok(written)
    }
}

pub(crate) fn read_head(f: &mut impl Read, n: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    let mut filled = 0;
    while filled < n {
        let r = f.read(&mut buf[filled..])?;
        if r == 0 {
            break;
        }
        filled += r;
    }
    buf.truncate(filled);
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn fs_in(dir: &Path) -> Arc<Filesystem> {
        Arc::new(Filesystem::new(
            dir.to_path_buf(),
            0,
            1000,
            1000,
            &["*.secret".into()],
            Duration::from_secs(60),
        ))
    }

    #[tokio::test]
    async fn listing_and_stat() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = fs_in(tmp.path());
        fs.write_bytes("b.txt".into(), b"hello".to_vec()).await.unwrap();
        fs.create_directory("adir".into()).await.unwrap();
        std::os::unix::fs::symlink("/etc/passwd", tmp.path().join("link")).unwrap();
        let list = fs.list_directory(".".into()).await.unwrap();
        let names: Vec<_> = list.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["adir", "b.txt", "link"]);
        assert!(list[0].directory && list[0].mode.starts_with('d'));
        assert_eq!(list[1].size, 5);
        assert_eq!(list[1].mime, "text/plain");
        assert!(list[2].symlink, "symlink reported, not followed");
        assert!(matches!(
            fs.list_directory("b.txt".into()).await,
            Err(FsError::NotDirectory)
        ));
    }

    #[tokio::test]
    async fn copy_naming_and_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = fs_in(tmp.path());
        fs.write_bytes("d/a.txt".into(), b"x".to_vec()).await.unwrap();
        assert_eq!(fs.copy("d/a.txt".into()).await.unwrap().name, "a copy.txt");
        assert_eq!(fs.copy("d/a.txt".into()).await.unwrap().name, "a copy 2.txt");
        assert!(matches!(
            fs.rename("d/a.txt".into(), "d/a copy.txt".into()).await,
            Err(FsError::Exists)
        ));
        fs.rename("d/a.txt".into(), "e/f/moved.txt".into()).await.unwrap();
        assert!(tmp.path().join("e/f/moved.txt").exists());
        assert!(matches!(
            fs.rename("e/f/moved.txt".into(), "x.secret".into()).await,
            Err(FsError::Denied)
        ));
    }

    #[tokio::test]
    async fn delete_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("keep"), "x").unwrap();
        let fs = fs_in(tmp.path());
        fs.write_bytes("dir/a".into(), vec![1; 10]).await.unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("out")).unwrap();
        assert!(fs.delete(vec![".".into()]).await.is_err());
        assert_eq!(fs.delete(vec!["dir".into(), "out".into(), "missing".into()]).await.unwrap(), 2);
        assert!(!tmp.path().join("dir").exists());
        assert!(outside.path().join("keep").exists(), "symlink target untouched");
    }

    #[tokio::test]
    async fn chmod_masks_bits() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let fs = fs_in(tmp.path());
        fs.write_bytes("f".into(), vec![]).await.unwrap();
        fs.chmod("f".into(), 0o4755).await.unwrap();
        let mode = std::fs::metadata(tmp.path().join("f")).unwrap().permissions().mode();
        assert_eq!(mode & 0o7777, 0o755);
    }

    #[tokio::test]
    async fn write_stream_limits() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = fs_in(tmp.path());
        let data = vec![7u8; 1000];
        assert_eq!(fs.write_stream("a".into(), &data[..], None).await.unwrap(), 1000);
        assert!(matches!(
            fs.write_stream("b".into(), &data[..], Some(10)).await,
            Err(FsError::TooLarge)
        ));
        assert!(!tmp.path().join("b").exists(), "partial file removed");
        fs.set_disk_limit(1500);
        assert!(matches!(
            fs.write_stream("c".into(), &data[..], None).await,
            Err(FsError::DiskSpace)
        ));
        assert!(!tmp.path().join("c").exists());
    }
}
