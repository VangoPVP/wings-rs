use super::ops::FileStat;
use super::{FsError, Filesystem, clean_path};
use futures_util::TryStreamExt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const MAX_REDIRECTS: usize = 5;

#[derive(Debug, thiserror::Error)]
pub enum PullError {
    #[error("invalid url: {0}")]
    InvalidUrl(String),
    #[error("destination is not allowed: {0}")]
    Blocked(String),
    #[error("remote server error: {0}")]
    Upstream(String),
    #[error(transparent)]
    Fs(#[from] FsError),
}

impl From<PullError> for crate::error::ApiError {
    fn from(e: PullError) -> Self {
        use axum::http::StatusCode;
        match e {
            PullError::InvalidUrl(_) | PullError::Blocked(_) => Self::bad_request(e.to_string()),
            PullError::Upstream(_) => Self::new(StatusCode::BAD_GATEWAY, e.to_string()),
            PullError::Fs(fe) => fe.into(),
        }
    }
}

fn v4_is_public(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || o[0] == 0
        || (o[0] == 100 && (o[1] & 0xc0) == 64)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 198 && (o[1] & 0xfe) == 18)
        || o[0] >= 240)
}

fn v6_is_public(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return v4_is_public(v4);
    }
    let s = ip.segments();
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (s[0] & 0xfe00) == 0xfc00
        || (s[0] & 0xffc0) == 0xfe80
        || (s[0] & 0xffc0) == 0xfec0
        || (s[0] == 0x2001 && s[1] == 0x0db8)
        || (s[0] == 0x0064 && s[1] == 0xff9b)
        || s[0] == 0x2002
        || (s[0] == 0 && s[1] == 0 && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0))
}

pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_is_public(v4),
        IpAddr::V6(v6) => v6_is_public(v6),
    }
}

async fn resolve_public(url: &reqwest::Url) -> Result<SocketAddr, PullError> {
    let host = url
        .host_str()
        .ok_or_else(|| PullError::InvalidUrl("missing host".into()))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| PullError::InvalidUrl("missing port".into()))?;
    let host_trimmed = host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<SocketAddr> = match host_trimmed.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => tokio::net::lookup_host((host_trimmed, port))
            .await
            .map_err(|e| PullError::InvalidUrl(format!("cannot resolve host: {e}")))?
            .collect(),
    };
    if addrs.is_empty() {
        return Err(PullError::InvalidUrl("host has no addresses".into()));
    }
    if let Some(bad) = addrs.iter().find(|a| !is_public_ip(a.ip())) {
        return Err(PullError::Blocked(format!("{host} resolves to {}", bad.ip())));
    }
    Ok(addrs[0])
}

fn sanitize_filename(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next()?.trim();
    if base.is_empty() || base == "." || base == ".." || base.contains('\0') || base.len() > 255 {
        return None;
    }
    Some(base.to_string())
}

fn filename_from_disposition(v: &str) -> Option<String> {
    for part in v.split(';').map(str::trim) {
        if let Some(rest) = part.strip_prefix("filename*=") {
            let name = rest.rsplit("''").next()?;
            return sanitize_filename(&percent_decode(name.trim_matches('"')));
        }
    }
    for part in v.split(';').map(str::trim) {
        if let Some(rest) = part.strip_prefix("filename=") {
            return sanitize_filename(rest.trim_matches('"'));
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub struct PullOptions {
    pub url: String,
    pub directory: PathBuf,
    pub filename: Option<String>,
    pub use_header: bool,
}

pub async fn pull(fs: &Arc<Filesystem>, opts: PullOptions) -> Result<FileStat, PullError> {
    let mut url = reqwest::Url::parse(&opts.url).map_err(|e| PullError::InvalidUrl(e.to_string()))?;
    let mut response = None;
    for _ in 0..=MAX_REDIRECTS {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(PullError::InvalidUrl("only http and https are supported".into()));
        }
        let addr = resolve_public(&url).await?;
        let host = url.host_str().unwrap_or_default().to_string();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .resolve(&host, addr)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60 * 30))
            .user_agent(concat!("wings-rs/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| PullError::Upstream(e.to_string()))?;
        let res = client
            .get(url.clone())
            .send()
            .await
            .map_err(|e| PullError::Upstream(e.to_string()))?;
        if res.status().is_redirection() {
            let loc = res
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| PullError::Upstream("redirect without location".into()))?;
            url = url
                .join(loc)
                .map_err(|e| PullError::InvalidUrl(format!("bad redirect: {e}")))?;
            continue;
        }
        response = Some(res);
        break;
    }
    let res = response.ok_or_else(|| PullError::Upstream("too many redirects".into()))?;
    if !res.status().is_success() {
        return Err(PullError::Upstream(format!("remote returned {}", res.status())));
    }

    let name = opts
        .filename
        .as_deref()
        .and_then(sanitize_filename)
        .or_else(|| {
            opts.use_header
                .then(|| {
                    res.headers()
                        .get(reqwest::header::CONTENT_DISPOSITION)
                        .and_then(|v| v.to_str().ok())
                        .and_then(filename_from_disposition)
                })
                .flatten()
        })
        .or_else(|| {
            url.path_segments()
                .and_then(|mut s| s.next_back().map(percent_decode))
                .and_then(|n| sanitize_filename(&n))
        })
        .unwrap_or_else(|| "download".into());
    let path = clean_path(&format!("{}/{}", opts.directory.display(), name))?;
    fs.check_denied(&path)?;
    if let Some(len) = res.content_length() {
        fs.ensure_space_for(len).await?;
    }

    let stream = res.bytes_stream().map_err(std::io::Error::other);
    let reader = tokio_util::io::StreamReader::new(stream);
    fs.write_stream(path.clone(), reader, None).await?;
    Ok(fs.stat(path).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ip_classification() {
        let blocked = [
            "127.0.0.1", "10.1.2.3", "172.16.0.1", "192.168.1.1", "169.254.169.254",
            "100.64.0.1", "0.0.0.0", "224.0.0.1", "255.255.255.255", "198.18.0.1",
            "::1", "::", "fc00::1", "fd12::1", "fe80::1", "::ffff:127.0.0.1",
            "::ffff:10.0.0.1", "64:ff9b::7f00:1", "2002:7f00:1::", "::127.0.0.1",
        ];
        for ip in blocked {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip} must be blocked");
        }
        for ip in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111", "::ffff:1.1.1.1"] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip} must be allowed");
        }
    }

    #[tokio::test]
    async fn rejects_internal_and_bad_schemes() {
        for u in ["http://127.0.0.1/x", "http://[::1]/x", "http://169.254.169.254/latest", "http://localhost/x"] {
            let url = reqwest::Url::parse(u).unwrap();
            assert!(matches!(resolve_public(&url).await, Err(PullError::Blocked(_))), "{u}");
        }
        let tmp = tempfile::tempdir().unwrap();
        let fs = Arc::new(Filesystem::new(tmp.path().into(), 0, 1, 1, &[], Duration::from_secs(1)));
        let r = pull(
            &fs,
            PullOptions {
                url: "file:///etc/passwd".into(),
                directory: ".".into(),
                filename: None,
                use_header: false,
            },
        )
        .await;
        assert!(matches!(r, Err(PullError::InvalidUrl(_))));
    }

    #[test]
    fn filenames() {
        assert_eq!(sanitize_filename("a/b/../c.jar").as_deref(), Some("c.jar"));
        assert_eq!(sanitize_filename(".."), None);
        assert_eq!(sanitize_filename(""), None);
        assert_eq!(
            filename_from_disposition("attachment; filename=\"plugin.jar\"").as_deref(),
            Some("plugin.jar")
        );
        assert_eq!(
            filename_from_disposition("attachment; filename*=UTF-8''my%20file.zip").as_deref(),
            Some("my file.zip")
        );
        assert_eq!(filename_from_disposition("attachment; filename=\"../../etc\"").as_deref(), Some("etc"));
    }
}
