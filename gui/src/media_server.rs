//! Loopback HTTP server that streams thumbnails and media to the web view.
//!
//! * binds 127.0.0.1 on a random port; every URL carries a random per-session
//!   token and the `Host` header must match (defends against DNS rebinding),
//! * full `Range` support so `<video>` seeking works through WebKitGTK/GStreamer,
//! * works for `<img>`, `<video>`, `<audio>`, PDFs and `fetch()` alike.

use clouddrive_core::CoreEngine;
use percent_encoding::percent_decode_str;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Debug, PartialEq)]
pub enum RangeReq {
    From(u64, Option<u64>),
    Suffix(u64),
}

/// Parse `Range: bytes=a-b | a- | -n` (only the first range is honoured).
pub fn parse_range(h: &str) -> Option<RangeReq> {
    let spec = h.trim().strip_prefix("bytes=")?.split(',').next()?.trim();
    let (a, b) = spec.split_once('-')?;
    if a.is_empty() {
        return b.trim().parse().ok().map(RangeReq::Suffix);
    }
    let start: u64 = a.trim().parse().ok()?;
    let end = if b.trim().is_empty() { None } else { Some(b.trim().parse().ok()?) };
    if matches!(end, Some(e) if e < start) {
        return None;
    }
    Some(RangeReq::From(start, end))
}

/// Start the server; returns the base URL (`http://127.0.0.1:PORT/TOKEN`).
pub async fn start(engine: Arc<CoreEngine>) -> std::io::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let token: String = {
        use rand::RngCore;
        let mut b = [0u8; 24];
        rand::rngs::OsRng.fill_bytes(&mut b);
        b.iter().map(|x| format!("{x:02x}")).collect()
    };
    let base = format!("http://127.0.0.1:{port}/{token}");
    let ctx = Arc::new((engine, port, token));
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else { break };
            let ctx = ctx.clone();
            tokio::spawn(async move {
                let _ = handle(sock, ctx).await;
            });
        }
    });
    Ok(base)
}

struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

fn text(status: u16, msg: &str) -> Reply {
    Reply {
        status,
        headers: vec![("Content-Type", "text/plain; charset=utf-8".into())],
        body: msg.as_bytes().to_vec(),
    }
}

async fn handle(mut sock: TcpStream, ctx: Arc<(Arc<CoreEngine>, u16, String)>) -> std::io::Result<()> {
    let (engine, port, token) = (&ctx.0, ctx.1, &ctx.2);
    let mut buf = Vec::with_capacity(1024);
    let read = tokio::time::timeout(Duration::from_secs(10), async {
        let mut chunk = [0u8; 2048];
        loop {
            let n = sock.read(&mut chunk).await?;
            if n == 0 {
                return Ok::<bool, std::io::Error>(false);
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                return Ok(true);
            }
            if buf.len() > 16 * 1024 {
                return Ok(false);
            }
        }
    })
    .await;
    if !matches!(read, Ok(Ok(true))) {
        return Ok(()); // idle / oversized / aborted
    }

    let head = String::from_utf8_lossy(&buf).to_string();
    let mut lines = head.lines();
    let mut first = lines.next().unwrap_or_default().split_whitespace();
    let (method, target) = (first.next().unwrap_or(""), first.next().unwrap_or("/"));
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())))
        .collect();

    let head_only = method == "HEAD";
    let reply = if method != "GET" && !head_only {
        text(405, "method not allowed")
    } else if headers.get("host").map(String::as_str) != Some(&format!("127.0.0.1:{port}")) {
        text(403, "forbidden")
    } else {
        route(engine, token, target, headers.get("range").map(String::as_str)).await
    };
    write_reply(&mut sock, reply, head_only).await
}

async fn route(engine: &CoreEngine, token: &str, target: &str, range: Option<&str>) -> Reply {
    let Ok(url) = url::Url::parse(&format!("http://127.0.0.1{target}")) else { return text(400, "bad request") };
    let segs: Vec<String> = url
        .path_segments()
        .map(|s| s.map(|p| percent_decode_str(p).decode_utf8_lossy().to_string()).collect())
        .unwrap_or_default();
    if segs.len() != 3 || segs[0] != token {
        return text(404, "not found");
    }
    let (kind, id) = (segs[1].as_str(), segs[2].as_str());

    match kind {
        "thumb" => {
            let size = url
                .query_pairs()
                .find(|(k, _)| k == "s")
                .and_then(|(_, v)| v.parse::<u32>().ok())
                .unwrap_or(256);
            match engine.thumbnail(id, size).await {
                Ok((bytes, mime)) => Reply {
                    status: 200,
                    headers: vec![
                        ("Content-Type", mime),
                        ("Cache-Control", "private, max-age=86400".into()),
                    ],
                    body: bytes.to_vec(),
                },
                Err(e) => text(404, &e.to_string()),
            }
        }
        "media" => {
            let req = range.and_then(parse_range);
            let total_hint = engine.get_node(id).map(|n| n.size).unwrap_or(0);
            let r = match req {
                None => None,
                Some(RangeReq::From(s, e)) => Some((s, e)),
                Some(RangeReq::Suffix(n)) => Some((total_hint.saturating_sub(n), None)),
            };
            match engine.fetch_media(id, r).await {
                Ok(m) => {
                    let mime = if m.mime.starts_with("text/") && !m.mime.contains("charset") {
                        format!("{}; charset=utf-8", m.mime)
                    } else {
                        m.mime.clone()
                    };
                    let mut headers = vec![
                        ("Content-Type", mime),
                        ("Accept-Ranges", "bytes".into()),
                        ("Cache-Control", "private, max-age=300".into()),
                    ];
                    if m.bytes.is_empty() && m.start >= m.total && m.total > 0 {
                        headers.push(("Content-Range", format!("bytes */{}", m.total)));
                        return Reply { status: 416, headers, body: vec![] };
                    }
                    let status = if m.partial {
                        let end = m.start + m.bytes.len() as u64 - 1;
                        headers.push(("Content-Range", format!("bytes {}-{}/{}", m.start, end, m.total)));
                        206
                    } else {
                        200
                    };
                    Reply { status, headers, body: m.bytes.to_vec() }
                }
                Err(e) => text(404, &e.to_string()),
            }
        }
        _ => text(404, "not found"),
    }
}

async fn write_reply(sock: &mut TcpStream, r: Reply, head_only: bool) -> std::io::Result<()> {
    let reason = match r.status {
        200 => "OK",
        206 => "Partial Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        416 => "Range Not Satisfiable",
        _ => "Error",
    };
    let mut out = format!("HTTP/1.1 {} {}\r\nContent-Length: {}\r\n", r.status, reason, r.body.len());
    for (k, v) in &r.headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("X-Content-Type-Options: nosniff\r\nCross-Origin-Resource-Policy: cross-origin\r\nConnection: close\r\n\r\n");
    sock.write_all(out.as_bytes()).await?;
    if !head_only {
        sock.write_all(&r.body).await?;
    }
    sock.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_header_parsing() {
        assert_eq!(parse_range("bytes=0-99"), Some(RangeReq::From(0, Some(99))));
        assert_eq!(parse_range("bytes=500-"), Some(RangeReq::From(500, None)));
        assert_eq!(parse_range("bytes=-200"), Some(RangeReq::Suffix(200)));
        assert_eq!(parse_range("bytes=0-9, 20-29"), Some(RangeReq::From(0, Some(9))));
        assert_eq!(parse_range("bytes=9-0"), None);
        assert_eq!(parse_range("items=0-9"), None);
        assert_eq!(parse_range("garbage"), None);
    }

    use clouddrive_core::cache::MetadataCache;
    use clouddrive_core::content_cache::ContentCache;
    use clouddrive_core::engine::EngineDirs;
    use clouddrive_core::{AppConfig, DriveFile, Endpoints, SecretStore};

    const MTIME: &str = "2024-01-01T00:00:00Z";
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 9, 9, 9];

    fn file(id: &str, name: &str, mime: &str, size: u64) -> DriveFile {
        DriveFile {
            id: id.into(), name: name.into(), mime_type: mime.into(), parents: vec!["ROOT".into()],
            size: Some(size), modified_time: MTIME.into(), md5_checksum: None, trashed: false,
            is_folder: false, thumbnail_link: None, owned_by_me: true,
        }
    }

    /// Engine with everything pre-cached on disk: no network involved.
    async fn server() -> (String, u16, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        let (data, cache) = (d.path().join("data"), d.path().join("cache"));
        let store = Arc::new(SecretStore::file_backed(d.path().join("cfg")));
        store.set_refresh_token("a@x.com", "rt").unwrap();
        let mc = MetadataCache::new(data.join("metadata.db")).unwrap();
        mc.upsert_files("a@x.com", &[file("F1", "clip.mp4", "video/mp4", 100), file("P1", "pic.jpg", "image/jpeg", 7)]).unwrap();
        let content = ContentCache::new(cache.join("content"), 1 << 20).unwrap();
        let bytes: Vec<u8> = (0..100u8).collect();
        content.put("file:a@x.com:F1", &bytes, "", MTIME).unwrap();
        let thumbs = ContentCache::new(cache.join("thumbs"), 1 << 20).unwrap();
        thumbs.put("thumb:a@x.com:P1:256", JPEG, "", MTIME).unwrap();
        drop((mc, content, thumbs));

        let engine = clouddrive_core::CoreEngine::with_parts(
            AppConfig::default(), Endpoints::local("http://127.0.0.1:1"), store, EngineDirs { data, cache },
        ).unwrap();
        let base = start(Arc::new(engine)).await.unwrap();
        let port: u16 = base.split(':').nth(2).unwrap().split('/').next().unwrap().parse().unwrap();
        (base, port, d)
    }

    async fn request(port: u16, method: &str, path: &str, host: Option<String>, extra: &str) -> (u16, String, Vec<u8>) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let host = host.unwrap_or(format!("127.0.0.1:{port}"));
        s.write_all(format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\n{extra}\r\n").as_bytes()).await.unwrap();
        let mut raw = Vec::new();
        s.read_to_end(&mut raw).await.unwrap();
        let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&raw[..split]).to_string();
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        (status, head, raw[split + 4..].to_vec())
    }

    #[tokio::test]
    async fn serves_media_with_range_semantics() {
        let (base, port, _d) = server().await;
        let tok = base.rsplit('/').next().unwrap().to_string();
        let all: Vec<u8> = (0..100u8).collect();

        let (st, head, body) = request(port, "GET", &format!("/{tok}/media/pool_F1"), None, "").await;
        assert_eq!(st, 200); assert_eq!(body, all);
        assert!(head.to_lowercase().contains("content-type: video/mp4"));
        assert!(head.to_lowercase().contains("content-length: 100"));

        let (st, head, body) = request(port, "GET", &format!("/{tok}/media/pool_F1"), None, "Range: bytes=10-19\r\n").await;
        assert_eq!(st, 206); assert_eq!(body, &all[10..20]);
        assert!(head.contains("Content-Range: bytes 10-19/100"), "{head}");

        let (st, head, body) = request(port, "GET", &format!("/{tok}/media/pool_F1"), None, "Range: bytes=90-\r\n").await;
        assert_eq!((st, body), (206, all[90..].to_vec())); assert!(head.contains("bytes 90-99/100"));

        let (st, head, body) = request(port, "GET", &format!("/{tok}/media/pool_F1"), None, "Range: bytes=-5\r\n").await;
        assert_eq!((st, body), (206, all[95..].to_vec())); assert!(head.contains("bytes 95-99/100"));

        let (st, head, _) = request(port, "GET", &format!("/{tok}/media/pool_F1"), None, "Range: bytes=500-600\r\n").await;
        assert_eq!(st, 416); assert!(head.contains("Content-Range: bytes */100"));

        let (st, head, body) = request(port, "HEAD", &format!("/{tok}/media/pool_F1"), None, "").await;
        assert_eq!(st, 200); assert!(body.is_empty()); assert!(head.to_lowercase().contains("content-length: 100"));
    }

    #[tokio::test]
    async fn serves_cached_thumbnails() {
        let (base, port, _d) = server().await;
        let tok = base.rsplit('/').next().unwrap().to_string();
        let (st, head, body) = request(port, "GET", &format!("/{tok}/thumb/pool_P1?s=256"), None, "").await;
        assert_eq!(st, 200); assert_eq!(body, JPEG);
        assert!(head.to_lowercase().contains("content-type: image/jpeg"));
    }

    #[tokio::test]
    async fn rejects_bad_token_host_method_and_unknown_ids() {
        let (base, port, _d) = server().await;
        let tok = base.rsplit('/').next().unwrap().to_string();
        let path = format!("/{tok}/media/pool_F1");
        assert_eq!(request(port, "GET", "/not-the-token/media/pool_F1", None, "").await.0, 404, "wrong token");
        assert_eq!(request(port, "GET", &path, Some("evil.example.com".into()), "").await.0, 403, "DNS-rebinding host");
        assert_eq!(request(port, "GET", &path, Some(format!("127.0.0.1:{}", port + 1)), "").await.0, 403);
        assert_eq!(request(port, "POST", &path, None, "Content-Length: 0\r\n").await.0, 405);
        assert_eq!(request(port, "GET", &format!("/{tok}/media/nope"), None, "").await.0, 404);
        assert_eq!(request(port, "GET", &format!("/{tok}/media/..%2F..%2Fetc%2Fpasswd"), None, "").await.0, 404);
        assert_eq!(request(port, "GET", &format!("/{tok}/other/pool_F1"), None, "").await.0, 404);
        // an idle pre-connect must not wedge the accept loop
        let _idle = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        assert_eq!(request(port, "GET", &path, None, "").await.0, 200);
    }
}
