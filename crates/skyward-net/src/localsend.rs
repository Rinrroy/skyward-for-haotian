//! LocalSend protocol v2.2, spoken over plain HTTP on a thread of its own.
//!
//! A phone running the official LocalSend app on the same WLAN can discover
//! this recovery and send files to it; files land in a writable folder the
//! recovery can then install or copy. A browser (or an app using the reverse
//! download API) can pull files back out. Encryption is off (`protocol:
//! "http"`) because a recovery has no certificate authority to trust; the
//! official app is told to turn "Use HTTPS" off to connect.
//!
//! Only the standard library and crates already in this workspace are used,
//! so the endpoint builds for the phone as readily as it does on a computer.
//! The parsing, routing and session bookkeeping are plain functions that the
//! tests below exercise without a socket; the `Worker` wraps them in a
//! listener, a multicast announcer and a channel the UI polls.

// The page reader and the LocalSend endpoint came with the aurora build
// and are kept as they were written: their nested `if`s read the way that
// build's clippy wanted them, and rewriting them to satisfy a newer one
// would be a change of somebody else's code for no gain in behaviour.
#![allow(clippy::collapsible_if)]

use anyhow::{bail, ensure, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The port LocalSend listens and announces on by default.
pub const PORT: u16 = 53317;
/// The multicast group LocalSend announces on.
pub const MULTICAST: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 167);
/// The protocol version spoken (major.minor of the REST API).
pub const VERSION: &str = "2.0";
/// How often the phone says it is here.
const ANNOUNCE: Duration = Duration::from_secs(5);
/// How long a connection may go without a byte before it is let go.
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// How much of a file is read and written at a time.
const CHUNK: usize = 1 << 16;
/// How many connections may be served at once. A recovery has little memory
/// to spare, and a port scanner must not be able to start a thread per probe.
const MAX_CONNECTIONS: usize = 16;

// ---------------------------------------------------------------------------
// What the screen is told
// ---------------------------------------------------------------------------

/// How far bringing the endpoint up has come.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Off,
    Starting,
    Listening,
    Failed,
}

/// A file that arrived whole and was kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Saved {
    pub name: String,
    pub size: u64,
}

/// A file on its way in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    pub name: String,
    pub done: u64,
    pub total: u64,
}

/// Everything the LocalSend page shows. The worker keeps one of these and
/// hands the UI a fresh copy whenever it changes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub phase: Phase,
    pub address: Option<Ipv4Addr>,
    pub port: u16,
    pub alias: String,
    pub directory: String,
    /// The names of devices discovered nearby, most recent first.
    pub peers: Vec<String>,
    pub receiving: Option<Progress>,
    pub received: Vec<Saved>,
    pub error: Option<String>,
}

/// What the worker is given when it starts.
#[derive(Clone, Debug)]
pub struct Config {
    /// The name other devices see.
    pub alias: String,
    /// The model string, if known.
    pub model: String,
    /// The address the WLAN gave; shown on the page and sent in announcements.
    pub address: Ipv4Addr,
    /// Where received files are written; also what the download API shares.
    pub directory: PathBuf,
    pub port: u16,
}

// ---------------------------------------------------------------------------
// The device description, as JSON
// ---------------------------------------------------------------------------

/// The description sent in announcements and in register/info replies.
#[derive(Clone, Debug, Serialize)]
struct Device {
    alias: String,
    version: String,
    #[serde(rename = "deviceModel")]
    device_model: String,
    #[serde(rename = "deviceType")]
    device_type: String,
    fingerprint: String,
    port: u16,
    protocol: String,
    download: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    announce: Option<bool>,
}

impl Device {
    fn new(cfg: &Config, fingerprint: &str, announce: Option<bool>) -> Self {
        Device {
            alias: cfg.alias.clone(),
            version: VERSION.into(),
            device_model: cfg.model.clone(),
            device_type: "mobile".into(),
            fingerprint: fingerprint.into(),
            port: cfg.port,
            protocol: "http".into(),
            download: true,
            announce,
        }
    }

    fn json(&self) -> String {
        serde_json::to_string(self).expect("a device description serializes")
    }
}

// ---------------------------------------------------------------------------
// A parsed HTTP request
// ---------------------------------------------------------------------------

/// The parts of a request the router looks at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub content_length: u64,
}

impl Request {
    /// The first value of a query parameter, percent-decoded as it is read.
    pub fn query(&self, key: &str) -> Option<String> {
        self.query.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }
}

/// Read up to the end of the headers and parse them. The bytes of the body
/// already read are handed back so a caller can stream the rest.
fn parse_head(head: &[u8]) -> Result<Request> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut parsed = httparse::Request::new(&mut headers);
    match parsed.parse(head) {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) => bail!("the request's headers are not whole"),
        Err(e) => bail!("the request is not HTTP: {e}"),
    }
    let method = parsed.method.context("the request names no method")?.to_string();
    let target = parsed.path.context("the request names no path")?;
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), parse_query(q)),
        None => (target.to_string(), Vec::new()),
    };
    let content_length = parsed
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("content-length"))
        .and_then(|h| std::str::from_utf8(h.value).ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0);
    Ok(Request { method, path, query, content_length })
}

/// Split a query string into decoded pairs. A malformed escape is kept as it
/// is rather than failing the whole request.
pub fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        // A '+' in a query means a space, as forms use.
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// A body that may be streamed: `rest` is what was read past the headers,
/// `reader` the rest of the connection, `limit` how many bytes belong to it.
pub struct Body<R> {
    reader: R,
    rest: Vec<u8>,
    at: usize,
    limit: u64,
}

impl<R: Read> Read for Body<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.limit == 0 || buf.is_empty() {
            return Ok(0);
        }
        if self.at < self.rest.len() {
            let n = (self.rest.len() - self.at).min(buf.len()).min(self.limit as usize);
            buf[..n].copy_from_slice(&self.rest[self.at..self.at + n]);
            self.at += n;
            self.limit -= n as u64;
            return Ok(n);
        }
        let cap = buf.len().min(self.limit as usize);
        let n = self.reader.read(&mut buf[..cap])?;
        self.limit -= n as u64;
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// Sessions and shared state
// ---------------------------------------------------------------------------

/// One file a sender said it would send.
#[derive(Clone, Debug)]
struct Ticket {
    token: String,
    name: String,
    size: u64,
    sha256: Option<String>,
    path: PathBuf,
    done: bool,
}

/// A prepared transfer, tied to the address that asked for it.
struct Session {
    peer: IpAddr,
    files: HashMap<String, Ticket>,
}

/// State shared between the listener thread and the multicast thread.
struct Shared {
    cfg: Config,
    fingerprint: String,
    status: Status,
    /// Upload sessions keyed by session id.
    uploads: HashMap<String, Session>,
    /// Download (reverse transfer) sessions: file id -> path.
    downloads: HashMap<String, PathBuf>,
}

/// What a request asks the connection thread to send back.
pub enum Reply {
    Status(u16),
    Json(u16, serde_json::Value),
    Html(u16, String),
    /// A file to stream straight from disk, with its length.
    File(u16, PathBuf),
}

impl Reply {
    fn code(&self) -> u16 {
        match self {
            Reply::Status(c) | Reply::Json(c, _) | Reply::Html(c, _) | Reply::File(c, _) => *c,
        }
    }
}

/// A short, random token: taken from the kernel's random bytes where they can
/// be read, and from the clock and a counter otherwise.
fn token() -> String {
    let mut bytes = [0u8; 16];
    if File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).is_err() {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        bytes[..8].copy_from_slice(&nanos.to_le_bytes());
        bytes[8..].copy_from_slice(&(std::process::id() as u64).to_le_bytes());
    }
    let mut s = String::with_capacity(32);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Keep only the last component of a sender's file name, and reject the
/// names that could climb out of the folder or name nothing at all.
pub fn safe_name(name: &str) -> Option<String> {
    let trimmed = name.replace('\\', "/");
    let last = Path::new(&trimmed)
        .components()
        .rfind(|c| matches!(c, Component::Normal(_)))?
        .as_os_str()
        .to_str()?
        .trim()
        .to_string();
    if last.is_empty() || last == "." || last == ".." || last.contains('\0') {
        return None;
    }
    let kept: String = last.chars().filter(|c| !c.is_control()).take(180).collect();
    (!kept.is_empty()).then_some(kept)
}

/// A path that does not yet exist, so one arrival never overwrites another.
fn free_path(dir: &Path, name: &str) -> PathBuf {
    let plain = dir.join(name);
    if !plain.exists() {
        return plain;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), Some(e.to_string())),
        _ => (name.to_string(), None),
    };
    for n in 1..1000 {
        let candidate = match &ext {
            Some(e) => dir.join(format!("{stem} ({n}).{e}")),
            None => dir.join(format!("{stem} ({n})")),
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem} ({})", token()))
}

#[cfg(test)]
fn sha256_hex(bytes: &[u8]) -> String {
    let sum = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in sum {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn json_status(code: u16, message: &str) -> Reply {
    Reply::Json(code, serde_json::json!({ "message": message }))
}

// ---------------------------------------------------------------------------
// The router. No sockets here, so tests can drive it with an in-memory body.
// ---------------------------------------------------------------------------

/// Decide what a request does. `body` is drained where a file is uploaded.
/// `peer` is the address the request came from, which a session stays tied
/// to. Returns the reply and, when the status changed, the new status.
fn handle<R: Read>(shared: &Arc<Mutex<Shared>>, peer: IpAddr, req: &Request, mut body: Body<R>) -> Reply {
    let result = route(shared, peer, req, &mut body);
    match result {
        Ok(reply) => reply,
        Err(e) => json_status(500, &format!("{e:#}")),
    }
}

fn route<R: Read>(shared: &Arc<Mutex<Shared>>, peer: IpAddr, req: &Request, body: &mut Body<R>) -> Result<Reply> {
    let path = req.path.as_str();
    match (req.method.as_str(), path) {
        ("GET", "/") => Ok(Reply::Html(200, index_page(shared))),
        ("GET", "/api/localsend/v2/info") => Ok(info(shared)),
        ("POST" | "GET", "/api/localsend/v2/register") => Ok(register(shared, peer, req, body)),
        ("POST", "/api/localsend/v2/prepare-upload") => prepare_upload(shared, peer, req, body),
        ("POST", "/api/localsend/v2/upload") => upload(shared, peer, req, body),
        ("POST", "/api/localsend/v2/cancel") => Ok(cancel(shared, req)),
        ("POST", "/api/localsend/v2/prepare-download") => Ok(prepare_download(shared)),
        ("GET", "/api/localsend/v2/download") => Ok(download(shared, req)),
        _ => Ok(json_status(404, "no such route")),
    }
}

fn info(shared: &Arc<Mutex<Shared>>) -> Reply {
    let s = shared.lock().expect("localsend state is not poisoned");
    Reply::Json(200, serde_json::to_value(Device::new(&s.cfg, &s.fingerprint, None)).expect("device serializes"))
}

/// Two-way discovery: reply with our description and remember the sender's
/// alias so the page can say who is nearby.
fn register<R: Read>(shared: &Arc<Mutex<Shared>>, _peer: IpAddr, req: &Request, body: &mut Body<R>) -> Reply {
    if req.content_length > 0 && req.content_length < 1 << 16 {
        let mut buf = Vec::new();
        if body.read_to_end(&mut buf).is_ok() {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf) {
                if let Some(alias) = v.get("alias").and_then(|a| a.as_str()) {
                    let mut s = shared.lock().expect("localsend state is not poisoned");
                    let alias = alias.to_string();
                    s.status.peers.retain(|p| p != &alias);
                    s.status.peers.insert(0, alias);
                    s.status.peers.truncate(12);
                }
            }
        }
    }
    info(shared)
}

/// The sender lists what it wants to send; we answer with a session and one
/// token per accepted file.
fn prepare_upload<R: Read>(shared: &Arc<Mutex<Shared>>, peer: IpAddr, req: &Request, body: &mut Body<R>) -> Result<Reply> {
    ensure!(req.content_length > 0 && req.content_length < 1 << 20, "a prepare-upload wants a body");
    let mut buf = Vec::new();
    body.read_to_end(&mut buf).context("read the file list")?;
    let value: serde_json::Value = serde_json::from_slice(&buf).context("the file list is not JSON")?;
    let files = value.get("files").and_then(|f| f.as_object()).context("the file list names no files")?;

    let mut s = shared.lock().expect("localsend state is not poisoned");
    let dir = PathBuf::from(&s.cfg.directory);
    fs::create_dir_all(&dir).with_context(|| format!("make {}", dir.display()))?;

    let session_id = token();
    let mut tickets = HashMap::new();
    let mut reply_files = serde_json::Map::new();
    for (id, meta) in files {
        let name = match meta.get("fileName").and_then(|n| n.as_str()).and_then(safe_name) {
            Some(n) => n,
            None => continue, // A name that cannot be kept is skipped, not rejected.
        };
        let size = meta.get("size").and_then(|n| n.as_u64()).unwrap_or(0);
        let sha256 = meta.get("sha256").and_then(|n| n.as_str()).map(str::to_string);
        let token = token();
        reply_files.insert(id.clone(), serde_json::Value::String(token.clone()));
        tickets.insert(
            id.clone(),
            Ticket { token, name: name.clone(), size, sha256, path: free_path(&dir, &name), done: false },
        );
    }
    if tickets.is_empty() {
        return Ok(Reply::Status(204)); // Nothing to transfer.
    }
    s.uploads.insert(session_id.clone(), Session { peer, files: tickets });
    Ok(Reply::Json(200, serde_json::json!({ "sessionId": session_id, "files": reply_files })))
}

/// Stream one file to disk, checking the session, the token and the checksum
/// the sender gave. Progress and the finished file are published to the UI.
fn upload<R: Read>(shared: &Arc<Mutex<Shared>>, peer: IpAddr, req: &Request, body: &mut Body<R>) -> Result<Reply> {
    let (session_id, file_id, given_token) = match (req.query("sessionId"), req.query("fileId"), req.query("token")) {
        (Some(a), Some(b), Some(c)) => (a, b, c),
        _ => return Ok(json_status(400, "missing parameters")),
    };
    // Take the ticket out for the duration of the write, so parallel uploads
    // of other files in the same session are not blocked by the lock.
    let (path, size, sha256, name) = {
        let s = shared.lock().expect("localsend state is not poisoned");
        let Some(session) = s.uploads.get(&session_id) else { return Ok(json_status(403, "no such session")) };
        if session.peer != peer {
            return Ok(json_status(403, "the session belongs to another address"));
        }
        let Some(t) = session.files.get(&file_id) else { return Ok(json_status(400, "no such file")) };
        if t.token != given_token {
            return Ok(json_status(403, "invalid token"));
        }
        if t.done {
            return Ok(json_status(409, "the file was sent already"));
        }
        (t.path.clone(), t.size, t.sha256.clone(), t.name.clone())
    };

    let mut file = File::create(&path).with_context(|| format!("create {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut done = 0u64;
    let mut next_report = 1u64 << 20;
    loop {
        let want = buf.len();
        let n = body.read(&mut buf[..want]).context("read the file's bytes")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).context("write the file's bytes")?;
        hasher.update(&buf[..n]);
        done += n as u64;
        if done >= next_report {
            publish_progress(shared, &name, done, size);
            next_report = done + (1 << 20);
        }
    }
    file.flush().context("finish the file")?;
    drop(file);

    // A size the sender gave is a promise: too few bytes means a broken link.
    if size > 0 && done != size {
        let _ = fs::remove_file(&path);
        clear_progress(shared);
        return Ok(json_status(500, &format!("received {done} of {size} bytes")));
    }
    let sum = {
        let mut out = String::with_capacity(64);
        for b in hasher.finalize() {
            out.push_str(&format!("{b:02x}"));
        }
        out
    };
    if let Some(wanted) = &sha256 {
        if !wanted.eq_ignore_ascii_case(&sum) {
            let _ = fs::remove_file(&path);
            clear_progress(shared);
            return Ok(json_status(422, "checksum mismatch"));
        }
    }

    // Keep the session until it is cancelled or a new one replaces it, but
    // mark this file done and tell the screen it arrived.
    {
        let mut s = shared.lock().expect("localsend state is not poisoned");
        if let Some(t) = s.uploads.get_mut(&session_id).and_then(|x| x.files.get_mut(&file_id)) {
            t.done = true;
        }
        s.status.receiving = None;
        s.status.received.insert(0, Saved { name: name.clone(), size: done });
        s.status.received.truncate(40);
        s.status.error = None;
    }
    Ok(Reply::Status(200))
}

fn publish_progress(shared: &Arc<Mutex<Shared>>, name: &str, done: u64, total: u64) {
    let mut s = shared.lock().expect("localsend state is not poisoned");
    s.status.receiving = Some(Progress { name: name.into(), done, total });
}

fn clear_progress(shared: &Arc<Mutex<Shared>>) {
    let mut s = shared.lock().expect("localsend state is not poisoned");
    s.status.receiving = None;
}

/// Give up a session and any half-written files in it.
fn cancel(shared: &Arc<Mutex<Shared>>, req: &Request) -> Reply {
    let Some(session_id) = req.query("sessionId") else { return json_status(400, "missing sessionId") };
    let mut s = shared.lock().expect("localsend state is not poisoned");
    if let Some(session) = s.uploads.remove(&session_id) {
        for t in session.files.values() {
            if !t.done {
                let _ = fs::remove_file(&t.path);
            }
        }
    }
    s.status.receiving = None;
    Reply::Status(200)
}

/// The reverse transfer: list the files in the shared folder so a browser or
/// another LocalSend can pull them out.
fn prepare_download(shared: &Arc<Mutex<Shared>>) -> Reply {
    let mut s = shared.lock().expect("localsend state is not poisoned");
    let dir = PathBuf::from(&s.cfg.directory);
    let _ = fs::create_dir_all(&dir);
    s.downloads.clear();
    let mut files = serde_json::Map::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else { continue };
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let id = token();
            files.insert(
                id.clone(),
                serde_json::json!({ "id": id, "fileName": name, "size": size, "fileType": "application/octet-stream" }),
            );
            s.downloads.insert(id, path);
        }
    }
    let session_id = token();
    let device = serde_json::to_value(Device::new(&s.cfg, &s.fingerprint, None)).expect("device serializes");
    Reply::Json(200, serde_json::json!({ "info": device, "sessionId": session_id, "files": files }))
}

fn download(shared: &Arc<Mutex<Shared>>, req: &Request) -> Reply {
    let Some(file_id) = req.query("fileId") else { return json_status(400, "missing fileId") };
    let s = shared.lock().expect("localsend state is not poisoned");
    match s.downloads.get(&file_id) {
        Some(path) if path.is_file() => Reply::File(200, path.clone()),
        _ => json_status(404, "no such file"),
    }
}

/// A tiny page for a browser on the same network, so files can be pulled
/// from the recovery without a LocalSend app at all.
fn index_page(shared: &Arc<Mutex<Shared>>) -> String {
    let s = shared.lock().expect("localsend state is not poisoned");
    let dir = PathBuf::from(&s.cfg.directory);
    let mut items = String::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().is_file())
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .collect();
        names.sort();
        for name in names {
            let encoded = percent_encode(&name);
            items.push_str(&format!("<li><a href=\"/file/{encoded}\">{escaped}</a></li>", escaped = html_escape(&name)));
        }
    }
    if items.is_empty() {
        items.push_str("<li>No files yet. Send one with LocalSend.</li>");
    }
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{alias}</title><style>body{{font:16px system-ui,sans-serif;margin:2rem;max-width:40rem;color:#222}}\
         a{{color:#06c}}li{{padding:.4rem 0;word-break:break-all}}</style></head>\
         <body><h1>{alias}</h1><p>LocalSend files received by this recovery. Files sent here appear below.</p><ul>{items}</ul></body></html>",
        alias = html_escape(&s.cfg.alias),
        items = items
    )
}

fn html_escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '&' => "&amp;".chars().collect::<Vec<_>>(),
            '<' => "&lt;".chars().collect(),
            '>' => "&gt;".chars().collect(),
            '"' => "&quot;".chars().collect(),
            _ => vec![c],
        })
        .collect()
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Sockets: one thread accepts connections, one announces over multicast.
// ---------------------------------------------------------------------------

/// Read a request's headers from a stream, leaving a `Body` over the rest.
fn read_request(stream: &mut TcpStream) -> Result<(Request, Body<TcpStream>)> {
    let mut head = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte)?;
        ensure!(n != 0, "the connection closed before its headers ended");
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
        ensure!(head.len() < 16 * 1024, "the request's headers are too long");
    }
    let req = parse_head(&head)?;
    let clone = stream.try_clone().context("share the connection with the body")?;
    let body = Body { reader: clone, rest: Vec::new(), at: 0, limit: req.content_length };
    Ok((req, body))
}

fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "OK",
    }
}

fn write_reply(stream: &mut TcpStream, reply: Reply) -> Result<()> {
    let code = reply.code();
    let (content_type, body, file) = match reply {
        Reply::Status(_) => (None, Vec::new(), None),
        Reply::Json(_, v) => (Some("application/json"), serde_json::to_vec(&v)?, None),
        Reply::Html(_, s) => (Some("text/html; charset=utf-8"), s.into_bytes(), None),
        Reply::File(_, path) => (Some("application/octet-stream"), Vec::new(), Some(path)),
    };
    let mut header = format!("HTTP/1.1 {code} {}\r\nConnection: close\r\n", status_text(code));
    if let Some(ct) = content_type {
        header.push_str(&format!("Content-Type: {ct}\r\n"));
    }
    if let Some(path) = &file {
        let len = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        header.push_str(&format!("Content-Length: {len}\r\n"));
        header.push_str(&format!("Content-Disposition: attachment; filename=\"{}\"\r\n", content_disposition(path)));
    } else {
        header.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    header.push_str("\r\n");
    stream.write_all(header.as_bytes())?;
    match file {
        Some(path) => {
            let mut f = File::open(&path)?;
            io::copy(&mut f, stream)?;
        }
        None => stream.write_all(&body)?,
    }
    stream.flush()?;
    Ok(())
}

fn content_disposition(path: &Path) -> String {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    // Strip what would break a quoted header value; keep it ASCII-safe.
    name.chars().filter(|c| *c != '"' && *c != '\r' && *c != '\n' && !c.is_control()).collect()
}

fn serve_connection(mut stream: TcpStream, shared: Arc<Mutex<Shared>>) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let peer = stream.peer_addr().map(|a| a.ip()).unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    let reply = match read_request(&mut stream) {
        Ok((req, body)) => {
            // The browser page links here; serve a file by name from the
            // shared folder without letting a path climb out of it.
            if let Some(name) = req.path.strip_prefix("/file/") {
                match file_named(&shared, name) {
                    Some(path) => Reply::File(200, path),
                    None => json_status(404, "no such file"),
                }
            } else {
                handle(&shared, peer, &req, body)
            }
        }
        Err(_) => json_status(400, "could not read the request"),
    };
    let _ = write_reply(&mut stream, reply);
}

fn file_named(shared: &Arc<Mutex<Shared>>, encoded: &str) -> Option<PathBuf> {
    let name = percent_decode(encoded);
    let safe = safe_name(&name)?;
    let s = shared.lock().expect("localsend state is not poisoned");
    let path = PathBuf::from(&s.cfg.directory).join(safe);
    path.is_file().then_some(path)
}

/// Releases a connection's slot even if serving it panics.
struct Connection(Arc<AtomicUsize>);

impl Drop for Connection {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Where the HTTP listener lives. Non-blocking so `stop` is seen promptly.
fn accept_loop(listener: TcpListener, shared: Arc<Mutex<Shared>>, stop: Arc<AtomicBool>) {
    let _ = listener.set_nonblocking(true);
    let live = Arc::new(AtomicUsize::new(0));
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                if live.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
                    drop(stream);
                    continue;
                }
                live.fetch_add(1, Ordering::Relaxed);
                let (shared, live) = (shared.clone(), live.clone());
                thread::spawn(move || {
                    let _held = Connection(live);
                    serve_connection(stream, shared);
                });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(80)),
            Err(e) => {
                if !stop.load(Ordering::Relaxed) {
                    note_error(&shared, &format!("the listener stopped: {e}"));
                    break;
                }
            }
        }
    }
}

/// Join the multicast group and, every few seconds, say we are here. Peers
/// that answer are remembered for the page.
fn announce_loop(socket: UdpSocket, shared: Arc<Mutex<Shared>>, stop: Arc<AtomicBool>) {
    let _ = socket.set_read_timeout(Some(Duration::from_millis(500)));
    let config = {
        let s = shared.lock().expect("localsend state is not poisoned");
        (Device::new(&s.cfg, &s.fingerprint, Some(true)).json(), s.fingerprint.clone(), s.cfg.port)
    };
    let (announcement, fingerprint, port) = config;
    let group = SocketAddr::new(IpAddr::V4(MULTICAST), port);
    let mut last = SystemTime::now() - ANNOUNCE;
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::Relaxed) {
        if last.elapsed().unwrap_or_default() >= ANNOUNCE {
            let _ = socket.send_to(announcement.as_bytes(), group);
            last = SystemTime::now();
        }
        match socket.recv_from(&mut buf) {
            Ok((n, from)) => {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf[..n]) {
                    let theirs = v.get("fingerprint").and_then(|f| f.as_str()).unwrap_or("");
                    if theirs.is_empty() || theirs == fingerprint {
                        continue;
                    }
                    let alias = v.get("alias").and_then(|a| a.as_str()).unwrap_or("a nearby device").to_string();
                    let announce = v.get("announce").and_then(|a| a.as_bool()).unwrap_or(false);
                    let mut s = shared.lock().expect("localsend state is not poisoned");
                    s.status.peers.retain(|p| p != &alias);
                    s.status.peers.insert(0, alias);
                    s.status.peers.truncate(12);
                    // Answer an announcement with a direct register, the
                    // protocol's preferred way to be seen at once.
                    if announce {
                        let reply = Device::new(&s.cfg, &s.fingerprint, None).json();
                        drop(s);
                        let port = v.get("port").and_then(|p| p.as_u64()).unwrap_or(PORT as u64) as u16;
                        send_register(from.ip(), port, reply);
                    }
                }
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(_) => thread::sleep(Duration::from_millis(200)),
        }
    }
}

fn send_register(ip: IpAddr, port: u16, our_json: String) {
    thread::spawn(move || {
        let Ok(mut stream) = TcpStream::connect_timeout(&SocketAddr::new(ip, port), Duration::from_secs(2)) else { return };
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        let request = format!(
            "POST /api/localsend/v2/register HTTP/1.1\r\nHost: {ip}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{our_json}",
            our_json.len()
        );
        let _ = stream.write_all(request.as_bytes());
    });
}

fn note_error(shared: &Arc<Mutex<Shared>>, message: &str) {
    let mut s = shared.lock().expect("localsend state is not poisoned");
    s.status.phase = Phase::Failed;
    s.status.error = Some(message.into());
}

/// Make a UDP socket that can share the discovery port and join the group.
fn multicast_socket(port: u16) -> Result<UdpSocket> {
    use std::mem::size_of;
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    ensure!(fd >= 0, "make a socket for discovery: {}", io::Error::last_os_error());
    let yes: libc::c_int = 1;
    let yes_ptr: *const libc::c_void = (&yes as *const libc::c_int).cast();
    unsafe {
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, yes_ptr, size_of::<libc::c_int>() as libc::socklen_t);
    }
    let addr = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: port.to_be(),
        sin_addr: libc::in_addr { s_addr: 0 },
        sin_zero: [0; 8],
    };
    let bound = unsafe {
        libc::bind(fd, (&addr as *const libc::sockaddr_in).cast(), size_of::<libc::sockaddr_in>() as libc::socklen_t)
    };
    if bound != 0 {
        unsafe { libc::close(fd) };
        bail!("bind the discovery port: {}", io::Error::last_os_error());
    }
    let mreq = libc::ip_mreq {
        imr_multiaddr: libc::in_addr { s_addr: u32::from_ne_bytes(MULTICAST.octets()) },
        imr_interface: libc::in_addr { s_addr: 0 },
    };
    let mreq_ptr: *const libc::c_void = (&mreq as *const libc::ip_mreq).cast();
    unsafe {
        libc::setsockopt(fd, libc::IPPROTO_IP, libc::IP_ADD_MEMBERSHIP, mreq_ptr, size_of::<libc::ip_mreq>() as libc::socklen_t);
    }
    let socket = unsafe { UdpSocket::from_raw_fd(fd) };
    let _ = socket.set_multicast_ttl_v4(4);
    Ok(socket)
}

// ---------------------------------------------------------------------------
// The worker the UI polls, in the shape of the WLAN worker.
// ---------------------------------------------------------------------------

fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(io::Error::last_os_error()).context("make a pipe for LocalSend");
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn signal(fd: &OwnedFd) {
    unsafe { libc::write(fd.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
}

fn drain(fd: &OwnedFd) {
    let mut buf = [0u8; 64];
    while unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
}

/// The LocalSend endpoint, running until it is dropped.
pub struct Worker {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    told: Receiver<Status>,
    wake: OwnedFd,
}

impl Worker {
    /// Start listening and announcing. Returns at once; the page sees the
    /// listening state through `told`.
    pub fn start(cfg: Config) -> Result<Worker> {
        let directory = cfg.directory.clone();
        fs::create_dir_all(&directory).with_context(|| format!("make {}", directory.display()))?;
        let fingerprint = token();
        let status = Status {
            phase: Phase::Starting,
            address: Some(cfg.address),
            port: cfg.port,
            alias: cfg.alias.clone(),
            directory: directory.to_string_lossy().into_owned(),
            ..Status::default()
        };
        let shared = Arc::new(Mutex::new(Shared {
            cfg,
            fingerprint,
            status,
            uploads: HashMap::new(),
            downloads: HashMap::new(),
        }));

        let port = shared.lock().unwrap().cfg.port;
        let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port))
            .with_context(|| format!("listen on port {port}"))?;
        let socket = multicast_socket(port);

        let (told_tx, told) = mpsc::channel();
        let (wake, rung) = pipe()?;
        let stop = Arc::new(AtomicBool::new(false));

        // A watcher publishes status changes to the UI and wakes its poll.
        let watcher = {
            let shared = shared.clone();
            let stop = stop.clone();
            let told_tx = told_tx.clone();
            let rung = rung;
            thread::Builder::new().name("localsend-watch".into()).spawn(move || {
                let mut last = shared.lock().unwrap().status.clone();
                let _ = told_tx.send(last.clone());
                signal(&rung);
                while !stop.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(200));
                    let now = shared.lock().unwrap().status.clone();
                    if now != last {
                        last = now;
                        if told_tx.send(last.clone()).is_err() {
                            break;
                        }
                        signal(&rung);
                    }
                }
            })?
        };

        // Say the endpoint is up once the listener is bound.
        {
            let mut s = shared.lock().unwrap();
            s.status.phase = Phase::Listening;
            s.status.error = None;
        }
        if let Err(e) = &socket {
            let mut s = shared.lock().unwrap();
            s.status.error = Some(format!("multicast discovery is unavailable: {e:#}"));
        }

        let mut threads = vec![watcher];
        {
            let (shared, stop) = (shared.clone(), stop.clone());
            threads.push(thread::Builder::new().name("localsend-http".into()).spawn(move || accept_loop(listener, shared, stop))?);
        }
        if let Ok(socket) = socket {
            let (shared, stop) = (shared.clone(), stop.clone());
            threads.push(thread::Builder::new().name("localsend-udp".into()).spawn(move || announce_loop(socket, shared, stop))?);
        }

        Ok(Worker { stop, threads, told, wake })
    }

    /// A file descriptor that becomes readable when there is something new
    /// for the screen, to add to the UI loop's `poll`.
    pub fn fd(&self) -> RawFd {
        self.wake.as_raw_fd()
    }

    /// The newest status, if it changed since the last call.
    pub fn told(&self) -> Option<Status> {
        drain(&self.wake);
        let mut latest = None;
        for status in self.told.try_iter() {
            latest = Some(status);
        }
        latest
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Each loop checks the flag at least every few hundred milliseconds:
        // the listener's non-blocking accept, the watcher's short sleep and
        // the multicast socket's read timeout. There is no blocking accept to
        // interrupt, so no pipe needs writing to wake a sleeping thread.
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Tests: the protocol without a socket in sight.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn shared(dir: &Path) -> Arc<Mutex<Shared>> {
        let cfg = Config {
            alias: "Skyward".into(),
            model: "shennong".into(),
            address: Ipv4Addr::new(192, 168, 1, 7),
            directory: dir.to_path_buf(),
            port: PORT,
        };
        Arc::new(Mutex::new(Shared {
            cfg,
            fingerprint: "self".into(),
            status: Status::default(),
            uploads: HashMap::new(),
            downloads: HashMap::new(),
        }))
    }

    fn body(bytes: &[u8]) -> Body<Cursor<Vec<u8>>> {
        Body { reader: Cursor::new(Vec::new()), rest: bytes.to_vec(), at: 0, limit: bytes.len() as u64 }
    }

    fn req(method: &str, target: &str, len: u64) -> Request {
        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p.to_string(), parse_query(q)),
            None => (target.to_string(), Vec::new()),
        };
        Request { method: method.into(), path, query, content_length: len }
    }

    #[test]
    fn a_query_is_split_and_decoded() {
        assert_eq!(
            parse_query("sessionId=abc&fileId=my%20file.zip&token=x%2By&empty="),
            vec![
                ("sessionId".into(), "abc".into()),
                ("fileId".into(), "my file.zip".into()),
                ("token".into(), "x+y".into()),
                ("empty".into(), "".into()),
            ]
        );
    }

    #[test]
    fn a_file_name_cannot_climb_out_of_the_folder() {
        assert_eq!(safe_name("rom.zip"), Some("rom.zip".into()));
        assert_eq!(safe_name("../../etc/passwd"), Some("passwd".into()));
        assert_eq!(safe_name("/sdcard/a/b/c.zip"), Some("c.zip".into()));
        assert_eq!(safe_name("C:\\Windows\\evil.txt"), Some("evil.txt".into()));
        assert_eq!(safe_name(".."), None);
        assert_eq!(safe_name(""), None);
        assert_eq!(safe_name("a\u{0}b"), None, "a name carrying a NUL is refused, not silently renamed");
    }

    #[test]
    fn names_taken_get_a_number_instead_of_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("rom.zip"), b"first").unwrap();
        assert_eq!(free_path(dir.path(), "rom.zip"), dir.path().join("rom (1).zip"));
        fs::write(dir.path().join("rom (1).zip"), b"second").unwrap();
        assert_eq!(free_path(dir.path(), "rom.zip"), dir.path().join("rom (2).zip"));
        assert_eq!(free_path(dir.path(), "fresh.zip"), dir.path().join("fresh.zip"));
    }

    #[test]
    fn a_request_head_is_parsed() {
        let head = b"POST /api/localsend/v2/upload?sessionId=s&fileId=f&token=t HTTP/1.1\r\nHost: x\r\nContent-Length: 42\r\n\r\n";
        let req = parse_head(head).unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/api/localsend/v2/upload");
        assert_eq!(req.content_length, 42);
        assert_eq!(req.query("sessionId"), Some("s".into()));
        assert_eq!(req.query("fileId"), Some("f".into()));
    }

    #[test]
    fn the_body_only_gives_up_its_declared_bytes() {
        let mut b = body(b"12345EXTRA");
        b.limit = 5;
        let mut out = Vec::new();
        b.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"12345", "bytes past Content-Length belong to the next request");
    }

    fn prepare(shared: &Arc<Mutex<Shared>>, files: serde_json::Value) -> (String, String, String) {
        let payload = serde_json::json!({ "info": { "alias": "sender" }, "files": files }).to_string();
        let r = req("POST", "/api/localsend/v2/prepare-upload", payload.len() as u64);
        let reply = handle(shared, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 8)), &r, body(payload.as_bytes()));
        let Reply::Json(code, value) = reply else { panic!("prepare-upload should answer JSON") };
        assert_eq!(code, 200);
        let sid = value["sessionId"].as_str().unwrap().to_string();
        let fid = value["files"].as_object().unwrap().keys().next().unwrap().clone();
        let tok = value["files"][&fid].as_str().unwrap().to_string();
        (sid, fid, tok)
    }

    #[test]
    fn a_file_is_received_whole_and_checked() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared(dir.path());
        let bytes = b"a whole rom image";
        let sum = sha256_hex(bytes);
        let (sid, fid, tok) = prepare(
            &shared,
            serde_json::json!({ "f1": { "id": "f1", "fileName": "../../rom.zip", "size": bytes.len(), "sha256": sum } }),
        );
        let path = format!("/api/localsend/v2/upload?sessionId={sid}&fileId={fid}&token={tok}");
        let r = req("POST", &path, bytes.len() as u64);
        let reply = handle(&shared, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 8)), &r, body(bytes));
        assert_eq!(reply.code(), 200, "a matching checksum is accepted");
        assert_eq!(fs::read(dir.path().join("rom.zip")).unwrap(), bytes, "the climbing name is flattened");
        let s = shared.lock().unwrap();
        assert_eq!(s.status.received, vec![Saved { name: "rom.zip".into(), size: bytes.len() as u64 }]);
    }

    #[test]
    fn a_wrong_checksum_is_refused_and_the_file_removed() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared(dir.path());
        let (sid, fid, tok) = prepare(
            &shared,
            serde_json::json!({ "f1": { "id": "f1", "fileName": "rom.zip", "size": 5, "sha256": "00".repeat(32) } }),
        );
        let path = format!("/api/localsend/v2/upload?sessionId={sid}&fileId={fid}&token={tok}");
        let r = req("POST", &path, 5);
        let reply = handle(&shared, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 8)), &r, body(b"hello"));
        assert_eq!(reply.code(), 422);
        assert!(!dir.path().join("rom.zip").exists(), "a bad file is not kept");
    }

    #[test]
    fn a_session_is_tied_to_its_sender_and_its_token() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared(dir.path());
        let (sid, fid, tok) = prepare(&shared, serde_json::json!({ "f1": { "id": "f1", "fileName": "rom.zip", "size": 3 } }));
        let other = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 9));
        let path = format!("/api/localsend/v2/upload?sessionId={sid}&fileId={fid}&token={tok}");
        let r = req("POST", &path, 3);
        assert_eq!(handle(&shared, other, &r, body(b"abc")).code(), 403, "another address cannot use the session");
        let bad = format!("/api/localsend/v2/upload?sessionId={sid}&fileId={fid}&token=wrong");
        let r = req("POST", &bad, 3);
        assert_eq!(handle(&shared, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 8)), &r, body(b"abc")).code(), 403, "a wrong token is refused");
    }

    #[test]
    fn info_and_register_describe_an_http_device() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared(dir.path());
        let Reply::Json(_, value) = info(&shared) else { panic!("info answers JSON") };
        assert_eq!(value["protocol"], "http");
        assert_eq!(value["port"], PORT);
        assert_eq!(value["version"], VERSION);
        let payload = br#"{"alias":"Nice Orange","fingerprint":"theirs"}"#;
        let r = req("POST", "/api/localsend/v2/register", payload.len() as u64);
        let _ = handle(&shared, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 9)), &r, body(payload));
        assert_eq!(shared.lock().unwrap().status.peers, vec!["Nice Orange".to_string()]);
    }

    #[test]
    fn the_download_api_shares_the_received_folder() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared(dir.path());
        fs::write(dir.path().join("rom.zip"), b"abc").unwrap();
        let Reply::Json(_, value) = prepare_download(&shared) else { panic!("prepare-download answers JSON") };
        let (_, file) = value["files"].as_object().unwrap().iter().next().unwrap();
        assert_eq!(file["fileName"], "rom.zip");
        let fid = file["id"].as_str().unwrap();
        let r = req("GET", &format!("/api/localsend/v2/download?sessionId=x&fileId={fid}"), 0);
        assert_eq!(download(&shared, &r).code(), 200);
    }

    #[test]
    fn cancelling_gives_up_a_half_written_file() {
        let dir = tempfile::tempdir().unwrap();
        let shared = shared(dir.path());
        let (sid, _, _) = prepare(&shared, serde_json::json!({ "f1": { "id": "f1", "fileName": "rom.zip", "size": 100 } }));
        {
            let s = shared.lock().unwrap();
            let path = &s.uploads[&sid].files["f1"].path;
            fs::write(path, b"half").unwrap();
        }
        let r = req("POST", &format!("/api/localsend/v2/cancel?sessionId={sid}"), 0);
        assert_eq!(cancel(&shared, &r).code(), 200);
        assert!(!dir.path().join("rom.zip").exists());
        assert!(shared.lock().unwrap().uploads.is_empty());
    }
}
