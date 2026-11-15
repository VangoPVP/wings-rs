use super::ops::{FileStat, read_head};
#[allow(unused_imports)]
use super::ops::FileStatExt;
use super::{FsError, FsResult, Filesystem, blocking, clean_path};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, Metadata, MetadataExt, OpenOptions, Permissions, PermissionsExt};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use ignore::gitignore::Gitignore;
use std::io::{self, Read, Seek, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    TarGz,
    Tar,
    Zip,
}

impl ArchiveFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "tar.gz" | "tgz" => Some(Self::TarGz),
            "tar" => Some(Self::Tar),
            "zip" => Some(Self::Zip),
            _ => None,
        }
    }
    fn extension(self) -> &'static str {
        match self {
            Self::TarGz => "tar.gz",
            Self::Tar => "tar",
            Self::Zip => "zip",
        }
    }
}

pub fn sniff(head: &[u8]) -> Option<ArchiveFormat> {
    if head.starts_with(&[0x1f, 0x8b]) {
        Some(ArchiveFormat::TarGz)
    } else if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        Some(ArchiveFormat::Zip)
    } else if head.len() >= 262 && &head[257..262] == b"ustar" {
        Some(ArchiveFormat::Tar)
    } else {
        None
    }
}

enum WalkEntry<'a> {
    Dir { path: &'a Path, meta: &'a Metadata },
    File { path: &'a Path, meta: &'a Metadata, file: &'a mut cap_std::fs::File },
}

struct Walker<'a> {
    abs_base: PathBuf,
    ignore: Option<&'a Gitignore>,
}

impl Walker<'_> {
    fn ignored(&self, archive_path: &Path, is_dir: bool) -> bool {
        self.ignore.is_some_and(|gi| {
            gi.matched_path_or_any_parents(self.abs_base.join(archive_path), is_dir)
                .is_ignore()
        })
    }

    fn visit(
        &self,
        dir: &Dir,
        name: &Path,
        archive_path: &Path,
        f: &mut dyn FnMut(WalkEntry) -> FsResult<()>,
    ) -> FsResult<()> {
        let meta = match dir.symlink_metadata(name) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        if meta.is_symlink() || self.ignored(archive_path, meta.is_dir()) {
            return Ok(());
        }
        if meta.is_dir() {
            f(WalkEntry::Dir { path: archive_path, meta: &meta })?;
            let sub = dir.open_dir_nofollow(name)?;
            let mut names: Vec<_> = sub.entries()?.flatten().map(|e| e.file_name()).collect();
            names.sort();
            for n in names {
                self.visit(&sub, Path::new(&n), &archive_path.join(&n), f)?;
            }
        } else if meta.is_file() {
            let mut opts = OpenOptions::new();
            opts.read(true).follow(FollowSymlinks::No);
            let mut file = dir.open_with(name, &opts)?;
            f(WalkEntry::File { path: archive_path, meta: &meta, file: &mut file })?;
        }
        Ok(())
    }

    fn walk(
        &self,
        base: &Dir,
        include: Option<&[PathBuf]>,
        f: &mut dyn FnMut(WalkEntry) -> FsResult<()>,
    ) -> FsResult<()> {
        let tops: Vec<PathBuf> = match include {
            Some(list) => list.to_vec(),
            None => {
                let mut v: Vec<PathBuf> =
                    base.entries()?.flatten().map(|e| PathBuf::from(e.file_name())).collect();
                v.sort();
                v
            }
        };
        for top in tops {
            if top == Path::new(".") {
                continue;
            }
            self.visit(base, &top, &top, f)?;
        }
        Ok(())
    }
}

fn open_base(fs: &Filesystem, base: &Path) -> FsResult<Dir> {
    let root = fs.open_root()?;
    if base == Path::new(".") {
        Ok(root)
    } else {
        Ok(root.open_dir(base)?)
    }
}

fn mtime(meta: &Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.into_std().duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn append_tar<W: Write>(
    fs: &Filesystem,
    base: &Path,
    include: Option<&[PathBuf]>,
    ignore: Option<&Gitignore>,
    tar: &mut tar::Builder<W>,
) -> FsResult<u64> {
    let base_dir = open_base(fs, base)?;
    let walker = Walker {
        abs_base: fs.root_path().join(base),
        ignore,
    };
    let mut count = 0u64;
    walker.walk(&base_dir, include, &mut |entry| {
        let mut h = tar::Header::new_gnu();
        match entry {
            WalkEntry::Dir { path, meta } => {
                h.set_entry_type(tar::EntryType::Directory);
                h.set_mode(meta.mode() & 0o7777);
                h.set_mtime(mtime(meta));
                h.set_size(0);
                tar.append_data(&mut h, path, io::empty())?;
            }
            WalkEntry::File { path, meta, file } => {
                h.set_entry_type(tar::EntryType::Regular);
                h.set_mode(meta.mode() & 0o7777);
                h.set_mtime(mtime(meta));
                h.set_size(meta.len());
                tar.append_data(&mut h, path, file.take(meta.len()))?;
            }
        }
        count += 1;
        Ok(())
    })?;
    Ok(count)
}

pub fn write_tar_gz<W: Write>(
    fs: &Filesystem,
    base: &Path,
    include: Option<&[PathBuf]>,
    ignore: Option<&Gitignore>,
    writer: W,
    level: u32,
) -> FsResult<W> {
    let gz = GzEncoder::new(writer, Compression::new(level.clamp(1, 9)));
    let mut tar = tar::Builder::new(gz);
    tar.follow_symlinks(false);
    append_tar(fs, base, include, ignore, &mut tar)?;
    let gz = tar.into_inner()?;
    Ok(gz.finish()?)
}

fn write_zip<W: Write + Seek>(
    fs: &Filesystem,
    base: &Path,
    include: Option<&[PathBuf]>,
    writer: W,
) -> FsResult<W> {
    let base_dir = open_base(fs, base)?;
    let walker = Walker {
        abs_base: fs.root_path().join(base),
        ignore: None,
    };
    let mut zip = zip::ZipWriter::new(writer);
    walker.walk(&base_dir, include, &mut |entry| {
        let zerr = |e: zip::result::ZipError| FsError::Io(io::Error::other(e));
        match entry {
            WalkEntry::Dir { path, meta } => {
                let opts = zip::write::SimpleFileOptions::default()
                    .unix_permissions(meta.mode() & 0o777);
                zip.add_directory(path.to_string_lossy(), opts).map_err(zerr)?;
            }
            WalkEntry::File { path, meta, file } => {
                let opts = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated)
                    .unix_permissions(meta.mode() & 0o777)
                    .large_file(meta.len() >= u32::MAX as u64);
                zip.start_file(path.to_string_lossy(), opts).map_err(zerr)?;
                io::copy(&mut file.take(meta.len()), &mut zip)?;
            }
        }
        Ok(())
    })?;
    zip.finish().map_err(|e| FsError::Io(io::Error::other(e)))
}

pub fn sanitize_entry(dest: &Path, raw: &Path) -> Option<PathBuf> {
    if raw.is_absolute() {
        return None;
    }
    for c in raw.components() {
        match c {
            Component::Normal(_) | Component::CurDir => {}
            _ => return None,
        }
    }
    let joined = dest.join(raw);
    let p = clean_path(&joined.to_string_lossy()).ok()?;
    (p != Path::new(".")).then_some(p)
}

struct Budget {
    limit: u64,
    used: u64,
    written: u64,
}

impl Budget {
    fn new(fs: &Filesystem) -> Self {
        Self {
            limit: fs.disk_limit(),
            used: fs.cached_usage(),
            written: 0,
        }
    }

    fn copy(&mut self, src: &mut impl Read, dst: &mut impl Write) -> FsResult<()> {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = src.read(&mut buf)?;
            if n == 0 {
                return Ok(());
            }
            self.written += n as u64;
            if self.limit > 0 && self.used + self.written > self.limit {
                return Err(FsError::DiskSpace);
            }
            dst.write_all(&buf[..n])?;
        }
    }
}

fn create_extracted_file(fs: &Filesystem, root: &Dir, rel: &Path, mode: u32) -> FsResult<cap_std::fs::File> {
    if let Some(parent) = rel.parent()
        && !parent.as_os_str().is_empty()
    {
        fs.create_dir_all_in(root, parent)?;
    }
    match root.symlink_metadata(rel) {
        Ok(m) if m.is_symlink() => root.remove_file(rel)?,
        Ok(m) if m.is_dir() => return Err(FsError::IsDirectory),
        _ => {}
    }
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true).follow(FollowSymlinks::No);
    let f = root.open_with(rel, &opts)?;
    fs.chown_file(&f)?;
    let mode = mode & 0o777;
    if mode != 0 {
        f.set_permissions(Permissions::from_mode(mode | 0o600))?;
    }
    Ok(f)
}

pub fn extract_tar<R: Read>(fs: &Filesystem, reader: R, dest: &Path) -> FsResult<u64> {
    let root = fs.open_root()?;
    fs.create_dir_all_in(&root, dest).or_else(|e| match e {
        FsError::InvalidPath(_) => Err(e),
        _ if dest == Path::new(".") => Ok(()),
        _ => Err(e),
    })?;
    let mut archive = tar::Archive::new(reader);
    let mut budget = Budget::new(fs);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let raw = entry.path()?.into_owned();
        let Some(rel) = sanitize_entry(dest, &raw) else {
            tracing::debug!(path = %raw.display(), "skipping unsafe archive entry");
            continue;
        };
        match entry.header().entry_type() {
            tar::EntryType::Directory => {
                if !fs.is_denied(&rel) {
                    fs.create_dir_all_in(&root, &rel)?;
                }
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                if fs.is_denied(&rel) {
                    continue;
                }
                let mode = entry.header().mode().unwrap_or(0o644);
                let mut f = create_extracted_file(fs, &root, &rel, mode)?;
                budget.copy(&mut entry, &mut f)?;
            }
            other => {
                tracing::debug!(path = %raw.display(), kind = ?other, "skipping non-regular archive entry");
            }
        }
    }
    fs.add_usage(budget.written as i64);
    Ok(budget.written)
}

pub fn extract_tar_gz<R: Read>(fs: &Filesystem, reader: R, dest: &Path) -> FsResult<u64> {
    extract_tar(fs, GzDecoder::new(reader), dest)
}

pub fn extract_zip<R: Read + Seek>(fs: &Filesystem, reader: R, dest: &Path) -> FsResult<u64> {
    let zerr = |e: zip::result::ZipError| match e {
        zip::result::ZipError::Io(io) => FsError::Io(io),
        other => FsError::Io(io::Error::new(io::ErrorKind::InvalidData, other)),
    };
    let root = fs.open_root()?;
    let mut archive = zip::ZipArchive::new(reader).map_err(zerr)?;
    let mut budget = Budget::new(fs);
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(zerr)?;
        let Some(name) = file.enclosed_name() else {
            tracing::debug!(name = ?file.name(), "skipping unsafe zip entry");
            continue;
        };
        let Some(rel) = sanitize_entry(dest, &name) else {
            continue;
        };
        if fs.is_denied(&rel) {
            continue;
        }
        let mode = file.unix_mode().unwrap_or(0o644);
        const S_IFMT: u32 = 0o170000;
        const S_IFLNK: u32 = 0o120000;
        if mode & S_IFMT == S_IFLNK {
            tracing::debug!(name = ?file.name(), "skipping zip symlink entry");
            continue;
        }
        if file.is_dir() {
            fs.create_dir_all_in(&root, &rel)?;
            continue;
        }
        let mut f = create_extracted_file(fs, &root, &rel, mode)?;
        budget.copy(&mut file, &mut f)?;
    }
    fs.add_usage(budget.written as i64);
    Ok(budget.written)
}

fn extract_auto(fs: &Filesystem, archive_path: &Path, dest: &Path) -> FsResult<u64> {
    let mut f = fs.open_read(archive_path)?;
    let head = read_head(&mut f, 512)?;
    let format = sniff(&head).ok_or(FsError::UnsupportedArchive)?;
    let f = fs.open_read(archive_path)?.into_std();
    match format {
        ArchiveFormat::TarGz => extract_tar_gz(fs, io::BufReader::new(f), dest),
        ArchiveFormat::Tar => extract_tar(fs, io::BufReader::new(f), dest),
        ArchiveFormat::Zip => extract_zip(fs, io::BufReader::new(f), dest),
    }
}

impl Filesystem {
    pub async fn compress(
        self: &Arc<Self>,
        root: PathBuf,
        files: Vec<PathBuf>,
        format: ArchiveFormat,
    ) -> FsResult<FileStat> {
        if !self.has_space_available(false).await {
            return Err(FsError::DiskSpace);
        }
        let name = format!(
            "archive-{}.{}",
            chrono::Utc::now().format("%Y-%m-%dT%H%M%S"),
            format.extension()
        );
        let target = root.join(&name);
        self.check_denied(&target)?;
        let this = self.clone();
        let target2 = target.clone();
        let size = blocking(move || {
            let include: Vec<PathBuf> = files;
            let out = this.open_write(&target2, true)?.into_std();
            let res = match format {
                ArchiveFormat::TarGz => {
                    write_tar_gz(&this, &root, Some(&include), None, io::BufWriter::new(out), 6)
                        .and_then(|w| w.into_inner().map_err(|e| FsError::Io(e.into_error())))
                }
                ArchiveFormat::Tar => {
                    let mut tar = tar::Builder::new(io::BufWriter::new(out));
                    tar.follow_symlinks(false);
                    append_tar(&this, &root, Some(&include), None, &mut tar)
                        .and_then(|_| Ok(tar.into_inner()?))
                        .and_then(|w| w.into_inner().map_err(|e| FsError::Io(e.into_error())))
                }
                ArchiveFormat::Zip => write_zip(&this, &root, Some(&include), out),
            };
            match res {
                Ok(f) => Ok(f.metadata()?.len()),
                Err(e) => {
                    let _ = this.open_root().and_then(|r| Ok(r.remove_file(&target2)?));
                    Err(e)
                }
            }
        })
        .await?;
        self.add_usage(size as i64);
        let limit = self.disk_limit();
        if limit > 0 && self.cached_usage() > limit {
            self.delete(vec![target]).await?;
            return Err(FsError::DiskSpace);
        }
        self.stat(target).await
    }

    pub async fn decompress(self: &Arc<Self>, root: PathBuf, file: PathBuf) -> FsResult<u64> {
        if !self.has_space_available(false).await {
            return Err(FsError::DiskSpace);
        }
        let this = self.clone();
        blocking(move || extract_auto(&this, &file, &root)).await
    }
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

    fn evil_tar(entries: &[(&str, tar::EntryType, &[u8], Option<&str>)]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (path, kind, data, link) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(*kind);
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            let name = h.as_old_mut().name.as_mut();
            name[..path.len()].copy_from_slice(path.as_bytes());
            if let Some(l) = link {
                h.set_link_name(l).unwrap();
            }
            h.set_cksum();
            b.append(&h, *data).unwrap();
        }
        b.into_inner().unwrap()
    }

    #[test]
    fn sanitize_rejects_escapes() {
        assert_eq!(sanitize_entry(Path::new("."), Path::new("a/b")), Some("a/b".into()));
        assert_eq!(sanitize_entry(Path::new("sub"), Path::new("./a")), Some("sub/a".into()));
        assert_eq!(sanitize_entry(Path::new("."), Path::new("../a")), None);
        assert_eq!(sanitize_entry(Path::new("."), Path::new("a/../../b")), None);
        assert_eq!(sanitize_entry(Path::new("."), Path::new("/etc/passwd")), None);
    }

    #[test]
    fn malicious_tar_is_contained() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let vol = tmp.path().join("vol");
        std::fs::create_dir(&vol).unwrap();
        let fs = fs_in(&vol);
        let target = outside.path().to_string_lossy().into_owned();
        let data = evil_tar(&[
            ("../evil.txt", tar::EntryType::Regular, b"x", None),
            ("/abs.txt", tar::EntryType::Regular, b"x", None),
            ("link", tar::EntryType::Symlink, b"", Some(&target)),
            ("link/pwned.txt", tar::EntryType::Regular, b"x", None),
            ("hard", tar::EntryType::Link, b"", Some("/etc/passwd")),
            ("ok/file.txt", tar::EntryType::Regular, b"hello", None),
            ("deny.secret", tar::EntryType::Regular, b"x", None),
        ]);
        let n = extract_tar(&fs, &data[..], Path::new(".")).unwrap();
        assert_eq!(n, 6, "only ok/file.txt (5) + link/pwned.txt as a real dir file (1)");
        assert_eq!(std::fs::read_to_string(vol.join("ok/file.txt")).unwrap(), "hello");
        assert!(!tmp.path().join("evil.txt").exists());
        assert!(!vol.join("deny.secret").exists());
        assert!(std::fs::symlink_metadata(vol.join("link")).map(|m| !m.file_type().is_symlink()).unwrap_or(true));
        assert!(!vol.join("hard").exists());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0, "nothing written outside");
    }

    #[test]
    fn existing_symlink_is_replaced_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("victim");
        std::fs::write(&victim, "original").unwrap();
        let vol = tmp.path().join("vol");
        std::fs::create_dir(&vol).unwrap();
        std::os::unix::fs::symlink(&victim, vol.join("config.yml")).unwrap();
        let fs = fs_in(&vol);
        let data = evil_tar(&[("config.yml", tar::EntryType::Regular, b"new", None)]);
        extract_tar(&fs, &data[..], Path::new(".")).unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "original");
        assert_eq!(std::fs::read_to_string(vol.join("config.yml")).unwrap(), "new");
    }

    #[test]
    fn disk_limit_stops_bomb() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = fs_in(tmp.path());
        fs.set_disk_limit(1000);
        let big = vec![0u8; 5000];
        let data = evil_tar(&[("bomb.bin", tar::EntryType::Regular, &big, None)]);
        assert!(matches!(
            extract_tar(&fs, &data[..], Path::new(".")),
            Err(FsError::DiskSpace)
        ));
    }

    #[test]
    fn zip_traversal_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let vol = tmp.path().join("vol");
        std::fs::create_dir(&vol).unwrap();
        let fs = fs_in(&vol);
        let mut buf = io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let o = zip::write::SimpleFileOptions::default();
            z.start_file("../escape.txt", o).unwrap();
            z.write_all(b"x").unwrap();
            z.start_file("fine/a.txt", o).unwrap();
            z.write_all(b"ok").unwrap();
            z.finish().unwrap();
        }
        buf.set_position(0);
        extract_zip(&fs, buf, Path::new(".")).unwrap();
        assert!(!tmp.path().join("escape.txt").exists());
        assert_eq!(std::fs::read_to_string(vol.join("fine/a.txt")).unwrap(), "ok");
    }

    #[tokio::test]
    async fn compress_roundtrip_skips_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = fs_in(tmp.path());
        fs.write_bytes("src/a.txt".into(), b"aaa".to_vec()).await.unwrap();
        fs.write_bytes("src/sub/b.txt".into(), b"bb".to_vec()).await.unwrap();
        std::os::unix::fs::symlink("/etc/passwd", tmp.path().join("src/pw")).unwrap();
        for fmt in [ArchiveFormat::TarGz, ArchiveFormat::Zip, ArchiveFormat::Tar] {
            let stat = fs.compress(".".into(), vec!["src".into()], fmt).await.unwrap();
            assert!(stat.name.starts_with("archive-"));
            let out = format!("out-{}", fmt.extension());
            fs.create_directory(out.clone().into()).await.unwrap();
            fs.decompress(out.clone().into(), stat.name.clone().into()).await.unwrap();
            let base = tmp.path().join(&out);
            assert_eq!(std::fs::read_to_string(base.join("src/a.txt")).unwrap(), "aaa");
            assert_eq!(std::fs::read_to_string(base.join("src/sub/b.txt")).unwrap(), "bb");
            assert!(!base.join("src/pw").exists(), "symlink not archived ({fmt:?})");
            fs.delete(vec![stat.name.into()]).await.unwrap();
        }
    }

    #[tokio::test]
    async fn decompress_rejects_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = fs_in(tmp.path());
        fs.write_bytes("x.rar".into(), b"Rar!\x1a\x07".to_vec()).await.unwrap();
        assert!(matches!(
            fs.decompress(".".into(), "x.rar".into()).await,
            Err(FsError::UnsupportedArchive)
        ));
    }

    #[test]
    fn tar_gz_ignore_patterns() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = fs_in(tmp.path());
        std::fs::create_dir_all(tmp.path().join("logs")).unwrap();
        std::fs::write(tmp.path().join("logs/x.log"), "l").unwrap();
        std::fs::write(tmp.path().join("keep.txt"), "k").unwrap();
        let mut b = ignore::gitignore::GitignoreBuilder::new(tmp.path());
        b.add_line(None, "logs/").unwrap();
        let gi = b.build().unwrap();
        let out = write_tar_gz(&fs, Path::new("."), None, Some(&gi), Vec::new(), 6).unwrap();
        let mut ar = tar::Archive::new(GzDecoder::new(&out[..]));
        let names: Vec<String> = ar
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["keep.txt"]);
    }
}
