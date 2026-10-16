//! A tiny in-process fake of Google's OAuth + Drive v3 endpoints (HTTP/1.1).
#![allow(dead_code)]

use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const FOLDER: &str = "application/vnd.google-apps.folder";
pub const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4, 5, 6, 7, 8];

#[derive(Clone)]
pub struct MFile {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub parent: String,
    pub content: Vec<u8>,
    pub trashed: bool,
    pub owned: bool,
}

pub struct MAccount {
    pub email: String,
    pub name: String,
    pub root: String,
    pub files: Vec<MFile>,
    pub log: Vec<Value>, // change log
    pub token_gen: u32,
    pub revoked: bool,
    pub usage: u64,
}

pub struct MState {
    pub base: String,
    pub accounts: HashMap<String, MAccount>,
    pub uploads: HashMap<String, (String, String, String, u64, Vec<u8>)>, // session -> (email,name,parent,total,data)
    pub next_id: u32,
    pub thumb_hits: u32,
    pub list_requests: u32,
    pub seen_prompt: Vec<String>,
}

pub type Shared = Arc<Mutex<MState>>;

pub struct Mock {
    pub addr: String,
    pub state: Shared,
}

fn file_json(base: &str, f: &MFile) -> Value {
    let mut v = json!({
        "id": f.id, "name": f.name, "mimeType": f.mime, "parents": [f.parent],
        "modifiedTime": "2024-05-01T10:00:00.000Z", "trashed": f.trashed, "ownedByMe": f.owned,
    });
    if f.mime != FOLDER {
        v["size"] = json!(f.content.len().to_string()); // Drive sends sizes as STRINGS
        let md5 = { use md5::Digest; format!("{:x}", md5::Md5::digest(&f.content)) };
        v["md5Checksum"] = json!(md5);
        if f.mime.starts_with("image/") {
            v["thumbnailLink"] = json!(format!("{base}/thumb/{}=s220", f.id));
        }
    }
    v
}

impl MAccount {
    pub fn new(email: &str, root: &str) -> Self {
        Self { email: email.into(), name: email.split('@').next().unwrap().to_uppercase(), root: root.into(),
               files: vec![], log: vec![], token_gen: 1, revoked: false, usage: 4_000_000 }
    }
    pub fn token(&self) -> String { format!("at-{}-{}", self.email, self.token_gen) }
    pub fn add(&mut self, base: &str, f: MFile) {
        self.log.push(json!({"fileId": f.id, "removed": false, "file": file_json(base, &f)}));
        self.files.retain(|x| x.id != f.id);
        self.files.push(f);
    }
    pub fn remove(&mut self, id: &str) {
        self.files.retain(|x| x.id != id);
        self.log.push(json!({"fileId": id, "removed": true}));
    }
}

pub fn mfile(id: &str, name: &str, parent: &str, content: &[u8]) -> MFile {
    let mime = if name.ends_with(".jpg") { "image/jpeg" } else { "text/plain" };
    MFile { id: id.into(), name: name.into(), mime: mime.into(), parent: parent.into(), content: content.to_vec(), trashed: false, owned: true }
}
pub fn mfolder(id: &str, name: &str, parent: &str) -> MFile {
    MFile { id: id.into(), name: name.into(), mime: FOLDER.into(), parent: parent.into(), content: vec![], trashed: false, owned: true }
}

struct Req { method: String, path: String, query: HashMap<String, String>, headers: HashMap<String, String>, body: Vec<u8> }
struct Resp { status: u16, headers: Vec<(String, String)>, body: Vec<u8> }

fn jr(status: u16, v: Value) -> Resp {
    Resp { status, headers: vec![("Content-Type".into(), "application/json".into())], body: v.to_string().into_bytes() }
}
fn err(status: u16, reason: &str, msg: &str) -> Resp {
    jr(status, json!({"error": {"code": status, "message": msg, "errors": [{"reason": reason}]}}))
}

impl Mock {
    pub async fn start() -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(MState {
            base: addr.clone(), accounts: HashMap::new(), uploads: HashMap::new(), next_id: 1000,
            thumb_hits: 0, list_requests: 0, seen_prompt: vec![],
        }));
        let st = state.clone();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else { break };
                let st = st.clone();
                tokio::spawn(async move { let _ = serve(sock, st).await; });
            }
        });
        Mock { addr, state }
    }
    pub fn with<R>(&self, f: impl FnOnce(&mut MState) -> R) -> R { f(&mut self.state.lock()) }
    pub fn add_account(&self, email: &str, root: &str) {
        self.with(|s| { s.accounts.insert(email.into(), MAccount::new(email, root)); });
    }
    pub fn add_file(&self, email: &str, f: MFile) {
        self.with(|s| { let base = s.base.clone(); s.accounts.get_mut(email).unwrap().add(&base, f); });
    }
}

async fn serve(mut sock: TcpStream, st: Shared) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let header_end;
    loop {
        let n = sock.read(&mut tmp).await?;
        if n == 0 { return Ok(()); }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") { header_end = p; break; }
    }
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let mut parts = lines.next().unwrap_or("").split_whitespace();
    let (method, target) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("/").to_string());
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string())))
        .collect();
    let clen: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < clen {
        let n = sock.read(&mut tmp).await?;
        if n == 0 { break; }
        body.extend_from_slice(&tmp[..n]);
    }
    let url = url::Url::parse(&format!("http://x{target}")).unwrap();
    let req = Req {
        method, path: url.path().to_string(), query: url.query_pairs().into_owned().collect(), headers, body,
    };
    let resp = { let mut g = st.lock(); route(&mut g, &req) };
    let reason = match resp.status { 200 => "OK", 206 => "Partial Content", 308 => "Resume Incomplete", 401 => "Unauthorized", _ => "X" };
    let mut out = format!("HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n", resp.status, reason, resp.body.len());
    for (k, v) in &resp.headers { out.push_str(&format!("{k}: {v}\r\n")); }
    out.push_str("\r\n");
    sock.write_all(out.as_bytes()).await?;
    sock.write_all(&resp.body).await?;
    sock.shutdown().await
}

fn form(body: &[u8]) -> HashMap<String, String> {
    url::form_urlencoded::parse(body).into_owned().collect()
}

fn auth_email(s: &MState, req: &Req) -> Result<String, Resp> {
    let tok = req.headers.get("authorization").and_then(|h| h.strip_prefix("Bearer ")).unwrap_or("");
    for a in s.accounts.values() {
        if a.token() == tok { return Ok(a.email.clone()); }
    }
    Err(err(401, "authError", "Invalid Credentials"))
}

fn route(s: &mut MState, req: &Req) -> Resp {
    let base = s.base.clone();
    let p = req.path.as_str();

    // ---- OAuth
    if p == "/oauth/token" {
        let f = form(&req.body);
        match f.get("grant_type").map(String::as_str) {
            Some("authorization_code") => {
                let code = f.get("code").cloned().unwrap_or_default();
                let Some(email) = code.strip_prefix("code-") else { return jr(400, json!({"error":"invalid_grant"})) };
                let Some(a) = s.accounts.get_mut(email) else { return jr(400, json!({"error":"invalid_grant"})) };
                a.token_gen += 1;
                return jr(200, json!({"access_token": a.token(), "refresh_token": format!("rt-{email}"), "expires_in": 3600}));
            }
            Some("refresh_token") => {
                let rt = f.get("refresh_token").cloned().unwrap_or_default();
                let Some(email) = rt.strip_prefix("rt-") else { return jr(400, json!({"error":"invalid_grant"})) };
                let Some(a) = s.accounts.get_mut(email) else { return jr(400, json!({"error":"invalid_grant"})) };
                if a.revoked { return jr(400, json!({"error":"invalid_grant","error_description":"Token has been expired or revoked."})); }
                a.token_gen += 1;
                return jr(200, json!({"access_token": a.token(), "expires_in": 3600}));
            }
            _ => return jr(400, json!({"error":"unsupported_grant_type"})),
        }
    }
    // ---- thumbnails (credentialed)
    if let Some(rest) = p.strip_prefix("/thumb/") {
        let email = match auth_email(s, req) { Ok(e) => e, Err(r) => return r };
        s.thumb_hits += 1;
        let id = rest.split('=').next().unwrap_or("");
        return if s.accounts[&email].files.iter().any(|f| f.id == id) {
            Resp { status: 200, headers: vec![("Content-Type".into(), "image/jpeg".into())], body: JPEG.to_vec() }
        } else { err(404, "notFound", "no thumb") };
    }
    // ---- resumable upload sessions
    if let Some(sess) = p.strip_prefix("/session/") {
        let Some((email, name, parent, total, data)) = s.uploads.get_mut(sess) else { return err(404, "notFound", "no session") };
        let cr = req.headers.get("content-range").cloned().unwrap_or_default();
        let range = cr.strip_prefix("bytes ").unwrap_or("");
        let (r, _tot) = range.split_once('/').unwrap_or(("*", "0"));
        if r != "*" {
            let (a, _b) = r.split_once('-').unwrap();
            assert_eq!(a.parse::<usize>().unwrap(), data.len(), "chunk must start where the previous ended");
            data.extend_from_slice(&req.body);
        }
        if data.len() as u64 == *total {
            let (email, name, parent, content) = (email.clone(), name.clone(), parent.clone(), data.clone());
            s.next_id += 1;
            let f = mfile(&format!("up{}", s.next_id), &name, &parent, &content);
            let j = file_json(&base, &f);
            s.accounts.get_mut(&email).unwrap().add(&base, f);
            return jr(200, j);
        }
        let got = data.len();
        return Resp { status: 308, headers: vec![("Range".into(), format!("bytes=0-{}", got - 1))], body: vec![] };
    }
    // ---- everything else needs a valid bearer
    let email = match auth_email(s, req) { Ok(e) => e, Err(r) => return r };
    let acct = s.accounts.get_mut(&email).unwrap();

    match (req.method.as_str(), p) {
        ("GET", "/drive/v3/about") => jr(200, json!({
            "user": {"displayName": acct.name, "emailAddress": acct.email},
            "storageQuota": {"limit": "16106127360", "usage": acct.usage.to_string(), "usageInDrive": "100"}})),
        ("GET", "/drive/v3/files/root") => jr(200, json!({"id": acct.root})),
        ("GET", "/drive/v3/changes/startPageToken") => jr(200, json!({"startPageToken": acct.log.len().to_string()})),
        ("GET", "/drive/v3/changes") => {
            let from: usize = req.query.get("pageToken").and_then(|t| t.parse().ok()).unwrap_or(0);
            if from > acct.log.len() { return err(404, "notFound", "Invalid pageToken"); }
            let to = (from + 2).min(acct.log.len());
            let mut v = json!({"changes": acct.log[from..to].to_vec()});
            if to < acct.log.len() { v["nextPageToken"] = json!(to.to_string()); }
            else { v["newStartPageToken"] = json!(to.to_string()); }
            jr(200, v)
        }
        ("GET", "/drive/v3/files") => {
            s.list_requests += 1;
            let acct = s.accounts.get_mut(&email).unwrap();
            let q = req.query.get("q").cloned().unwrap_or_default();
            let want_trashed = q.contains("trashed = true");
            let mut files: Vec<&MFile> = acct.files.iter().filter(|f| f.owned && f.trashed == want_trashed).collect();
            files.sort_by(|a, b| a.id.cmp(&b.id));
            let from: usize = req.query.get("pageToken").and_then(|t| t.parse().ok()).unwrap_or(0);
            let to = (from + 2).min(files.len()); // tiny pages force the client to paginate
            let mut v = json!({"files": files[from..to].iter().map(|f| file_json(&base, f)).collect::<Vec<_>>()});
            if to < files.len() { v["nextPageToken"] = json!(to.to_string()); }
            jr(200, v)
        }
        ("POST", "/drive/v3/files") => {
            let b: Value = serde_json::from_slice(&req.body).unwrap();
            s.next_id += 1;
            let id = format!("new{}", s.next_id);
            let acct = s.accounts.get_mut(&email).unwrap();
            let f = mfolder(&id, b["name"].as_str().unwrap(), b["parents"][0].as_str().unwrap());
            let j = file_json(&base, &f);
            acct.add(&base, f);
            jr(200, j)
        }
        ("POST", "/upload/drive/v3/files") => {
            let b: Value = serde_json::from_slice(&req.body).unwrap();
            let total: u64 = req.headers.get("x-upload-content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
            s.next_id += 1;
            let sess = format!("s{}", s.next_id);
            s.uploads.insert(sess.clone(), (email, b["name"].as_str().unwrap().into(), b["parents"][0].as_str().unwrap().into(), total, vec![]));
            Resp { status: 200, headers: vec![("Location".into(), format!("{base}/session/{sess}"))], body: vec![] }
        }
        (m, path) if path.starts_with("/drive/v3/files/") => {
            let id = path.trim_start_matches("/drive/v3/files/").to_string();
            let Some(idx) = acct.files.iter().position(|f| f.id == id) else { return err(404, "notFound", "File not found") };
            match m {
                "GET" if req.query.get("alt").map(String::as_str) == Some("media") => {
                    let data = &acct.files[idx].content;
                    if let Some(r) = req.headers.get("range").and_then(|r| r.strip_prefix("bytes=")) {
                        let (a, b) = r.split_once('-').unwrap();
                        let a: usize = a.parse().unwrap();
                        if a >= data.len() { return err(416, "requestedRangeNotSatisfiable", "range"); }
                        let b: usize = b.parse().map(|x: usize| x.min(data.len() - 1)).unwrap_or(data.len() - 1);
                        return Resp { status: 206,
                            headers: vec![("Content-Range".into(), format!("bytes {a}-{b}/{}", data.len()))],
                            body: data[a..=b].to_vec() };
                    }
                    Resp { status: 200, headers: vec![], body: data.clone() }
                }
                "GET" => jr(200, file_json(&base, &acct.files[idx])),
                "PATCH" => {
                    let b: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
                    let mut f = acct.files[idx].clone();
                    if let Some(n) = b["name"].as_str() { f.name = n.into(); }
                    if let Some(t) = b["trashed"].as_bool() { f.trashed = t; }
                    if let Some(np) = req.query.get("addParents") { f.parent = np.clone(); }
                    let j = file_json(&base, &f);
                    acct.add(&base, f);
                    jr(200, j)
                }
                "DELETE" => { acct.remove(&id); Resp { status: 204, headers: vec![], body: vec![] } }
                _ => err(405, "x", "method"),
            }
        }
        _ => err(404, "notFound", &format!("no route {} {}", req.method, p)),
    }
}
