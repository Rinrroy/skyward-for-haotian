//! A small browser for the recovery: a page's text and links, and the
//! files it offers, fetched over HTTP or HTTPS. The recovery draws the
//! text; what a page's scripts would build is not run, so a page shows as
//! the text and links its HTML carries.

use crate::download;
use anyhow::{bail, Context, Result};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use ureq::ResponseExt;

/// The most of a page that is read: a phone's recovery has little memory,
/// and a page longer than this is cut short with a note.
pub const MAX_PAGE: usize = 2 << 20;

/// The most blocks a page shows, and the most characters one block keeps.
pub const MAX_BLOCKS: usize = 400;
const MAX_BLOCK_CHARS: usize = 4000;

/// What the browser calls itself where a server asks.
const USER_AGENT: &str = concat!("Skyward/", env!("CARGO_PKG_VERSION"), " (recovery browser)");

/// How a block is set, from the page's own tags and a small part of CSS.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    /// An `#rrggbb` colour, where the page names one.
    pub color: Option<u32>,
    pub background: Option<u32>,
    pub align: Align,
    /// A size in page pixels, where the page names one.
    pub size: Option<u32>,
}

impl Style {
    fn merge(&mut self, other: Style) {
        self.bold |= other.bold;
        self.italic |= other.italic;
        self.underline |= other.underline;
        if other.color.is_some() {
            self.color = other.color;
        }
        if other.background.is_some() {
            self.background = other.background;
        }
        if other.align != Align::Left {
            self.align = other.align;
        }
        if other.size.is_some() {
            self.size = other.size;
        }
    }
}

/// A rule of the page's own stylesheet, as far as a reader can use one:
/// a tag, a class or an id, and the properties it sets.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
struct CssRule {
    tag: Option<String>,
    class: Option<String>,
    id: Option<String>,
    style: Style,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

/// A part of a page, as the pages show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    /// A heading, its level 1 to 6.
    Heading { level: u8, text: String, style: Style },
    Paragraph { text: String, style: Style },
    Bullet { text: String, style: Style },
    Quote { text: String, style: Style },
    /// Text whose spacing is kept, set in the monospaced face.
    Code { text: String, style: Style },
    Link { label: String, url: String, download: bool, style: Style },
    /// A picture, its bytes where the browser could read them.
    Image { alt: String, url: String, data: Option<Vec<u8>>, width: u32, height: u32, style: Style },
    /// A form of the page, by its place in [`Page::forms`].
    Form(usize),
    Rule,
}

/// One field of a form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormField {
    pub name: String,
    pub label: String,
    pub kind: FieldKind,
    pub value: String,
    pub placeholder: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    Password,
    Hidden,
    TextArea,
    Select(Vec<(String, String)>),
    Checkbox,
    Radio,
    Submit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Form {
    pub action: String,
    pub post: bool,
    pub fields: Vec<FormField>,
}

/// One cookie a server set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
}

/// A page read as text, pictures, forms and links.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    /// The address it was read from, after redirects.
    pub url: String,
    pub title: String,
    /// The host, shown over the title.
    pub site: String,
    pub description: Option<String>,
    pub blocks: Vec<Block>,
    pub forms: Vec<Form>,
    /// The cookies the answer set.
    pub cookies: Vec<Cookie>,
    /// The page was longer than [`MAX_PAGE`], or held more blocks than
    /// [`MAX_BLOCKS`], and was cut short.
    pub truncated: bool,
}

/// A file a server offers instead of a page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileOffer {
    pub url: String,
    pub name: String,
    pub size: Option<u64>,
    /// The type the server gave it, such as `application/zip`.
    pub kind: String,
}

/// What an address turned out to hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Opened {
    Page(Page),
    File(FileOffer),
}

/// How far a file has been saved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Saving {
    pub done: u64,
    /// The length the server gave, where it gave one.
    pub total: Option<u64>,
}

/// The address a search for `words` asks, a page of links the recovery can
/// read without scripts.
pub fn search_url(words: &str) -> String {
    format!("https://www.bing.com/search?format=rss&setlang=zh-Hans&q={}", percent_encode(words))
}

/// What typing `input` in the address field asks for: an address, or a
/// search for what was typed where it names no site.
pub fn address(input: &str) -> String {
    let text = input.trim();
    if text.is_empty() {
        return String::new();
    }
    if text.starts_with("http://") || text.starts_with("https://") {
        return text.to_string();
    }
    let host = text.split('/').next().unwrap_or(text);
    let host = host.split(':').next().unwrap_or(host);
    let named = host.contains('.')
        || host.eq_ignore_ascii_case("localhost")
        || host.starts_with("127.")
        || host.starts_with('[')
        || text.starts_with("//");
    if named {
        match text.strip_prefix("//") {
            Some(rest) => format!("https://{rest}"),
            None => format!("https://{text}"),
        }
    } else {
        search_url(text)
    }
}

/// Whether `url` names a page the browser can read, rather than anything
/// else.
pub fn readable_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// A page's or file's name as it would be saved, from the last part of its
/// address.
pub fn file_name(url: &str) -> String {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    let last = without_query.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let name = percent_decode(last);
    sanitize_name(if name.is_empty() { "download" } else { &name })
}

/// Whether a file is one the recovery installs from its pages: a zip or an
/// image.
pub fn installable(name: &str) -> bool {
    matches!(extension(name).as_str(), "zip" | "img")
}

/// Whether a link points at a file to save rather than a page to read.
pub fn download_name(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let ext = extension(path);
    let download = matches!(
        ext.as_str(),
        "zip" | "img" | "bin" | "apk" | "tar" | "gz" | "tgz" | "xz" | "7z" | "iso" | "jar" | "pdf" | "exe" | "dmg" | "deb" | "rpm"
            | "mp4" | "mkv" | "mp3" | "wav" | "flac" | "opus" | "ttf" | "otf" | "woff" | "woff2"
    );
    download.then(|| file_name(url))
}

fn extension(name: &str) -> String {
    name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}

/// Fetch `url`. A page comes back as its text, pictures, forms and links;
/// anything a server offers as a file comes back as what is needed to save
/// it, without its body being read.
pub fn open(agent: &ureq::Agent, url: &str) -> Result<Opened> {
    open_with(agent, url, None)
}

/// Fetch `url`, sending the cookies the browser keeps for it.
pub fn open_with(agent: &ureq::Agent, url: &str, cookie: Option<&str>) -> Result<Opened> {
    if !readable_url(url) {
        bail!("{url} is not an http or https address");
    }
    let mut request = agent
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.8");
    if let Some(cookie) = cookie {
        request = request.header("Cookie", cookie);
    }
    let response = request.call().with_context(|| format!("open {url}"))?;
    read_answer(agent, response, url)
}

/// Send a form, as the page asks: its fields in the address for a GET, and
/// in the body for a POST.
pub fn submit(
    agent: &ureq::Agent,
    url: &str,
    fields: &[(String, String)],
    post: bool,
    cookie: Option<&str>,
) -> Result<Opened> {
    if !readable_url(url) {
        bail!("{url} is not an http or https address");
    }
    let mut encoded = String::new();
    for (k, v) in fields {
        if !encoded.is_empty() {
            encoded.push('&');
        }
        encoded.push_str(&percent_encode(k));
        encoded.push('=');
        encoded.push_str(&percent_encode(v));
    }
    let target = if post {
        url.to_string()
    } else if url.contains('?') {
        format!("{url}&{encoded}")
    } else {
        format!("{url}?{encoded}")
    };
    let response = if post {
        let mut request = agent
            .post(&target)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("User-Agent", USER_AGENT)
            .header("Accept", "text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.8");
        if let Some(cookie) = cookie {
            request = request.header("Cookie", cookie);
        }
        request.send(encoded.as_bytes()).with_context(|| format!("submit {url}"))?
    } else {
        let mut request = agent
            .get(&target)
            .header("User-Agent", USER_AGENT)
            .header("Accept", "text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.8");
        if let Some(cookie) = cookie {
            request = request.header("Cookie", cookie);
        }
        request.call().with_context(|| format!("submit {url}"))?
    };
    read_answer(agent, response, url)
}

/// Read what a server answered, as [`open_with`] and [`submit`] do.
fn read_answer(agent: &ureq::Agent, response: ureq::http::Response<ureq::Body>, url: &str) -> Result<Opened> {
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        bail!("the server answered {status} for {url}");
    }
    let final_url = response.get_uri().to_string();
    let kind = header(&response, "content-type").unwrap_or_default();
    let length = crate::download::content_length(response.headers());
    let cookies = read_cookies(response.headers(), &final_url);
    let path_ext = extension(final_url.split(['?', '#']).next().unwrap_or(&final_url));
    let textual = kind.to_ascii_lowercase();
    let is_text = textual.contains("text/html")
        || textual.contains("application/xhtml")
        || textual.contains("text/xml")
        || textual.contains("application/xml")
        || textual.contains("application/rss")
        || textual.starts_with("text/plain")
        || (textual.is_empty() && download_name(&final_url).is_none())
        || matches!(path_ext.as_str(), "html" | "htm" | "xhtml" | "txt" | "rss");
    if !is_text {
        return Ok(Opened::File(FileOffer {
            name: disposition_name(&response).unwrap_or_else(|| file_name(&final_url)),
            url: final_url,
            size: length,
            kind: if kind.is_empty() { "application/octet-stream".into() } else { kind },
        }));
    }
    let mut reader = response.into_body().into_with_config().limit(u64::MAX).reader();
    let mut bytes = Vec::new();
    (&mut reader)
        .take(MAX_PAGE as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {url}"))?;
    let truncated = bytes.len() > MAX_PAGE;
    let body = &bytes[..bytes.len().min(MAX_PAGE)];
    let text = String::from_utf8_lossy(body).into_owned();
    let mut page = if textual.starts_with("text/plain") {
        plain_page(&final_url, &text, truncated)
    } else if textual.contains("rss") || textual.contains("xml") || text.trim_start().starts_with("<?xml") || text.contains("<rss") {
        rss_page(&final_url, &text, truncated)
    } else {
        parse(&final_url, &text, truncated)
    };
    page.cookies = cookies;
    fetch_images(agent, &mut page);
    Ok(Opened::Page(page))
}

/// Read the cookies an answer set, for the host it came from.
fn read_cookies(headers: &ureq::http::HeaderMap, url: &str) -> Vec<Cookie> {
    let host = host_of(url);
    let mut out = Vec::new();
    for value in headers.get_all("set-cookie") {
        let Ok(value) = value.to_str() else { continue };
        let mut parts = value.split(';');
        let Some(pair) = parts.next() else { continue };
        let Some((name, val)) = pair.split_once('=') else { continue };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let mut cookie = Cookie {
            name: name.to_string(),
            value: val.trim().to_string(),
            domain: host.clone(),
            path: "/".to_string(),
        };
        for attr in parts {
            let attr = attr.trim();
            if let Some(domain) = attr.strip_prefix("Domain=").or_else(|| attr.strip_prefix("domain=")) {
                cookie.domain = domain.trim_start_matches('.').to_string();
            } else if let Some(path) = attr.strip_prefix("Path=").or_else(|| attr.strip_prefix("path=")) {
                cookie.path = path.to_string();
            }
        }
        out.push(cookie);
    }
    out
}

/// The `Cookie` header for `url`, from the cookies kept.
pub fn cookie_header(cookies: &[(String, String, String, String)], url: &str) -> Option<String> {
    let host = host_of(url);
    let path = url.split("://").nth(1).and_then(|r| r.find('/').map(|i| &r[i..])).unwrap_or("/");
    let path = path.split(['?', '#']).next().unwrap_or("/");
    let pairs: Vec<String> = cookies
        .iter()
        .filter(|(_, _, domain, cookie_path)| {
            (host == *domain || host.ends_with(&format!(".{domain}"))) && path.starts_with(cookie_path.as_str())
        })
        .map(|(name, value, _, _)| format!("{name}={value}"))
        .collect();
    (!pairs.is_empty()).then(|| pairs.join("; "))
}

/// Read the pictures a page's HTML points at, so far and no further: a
/// recovery has little memory, and a picture that fails is left as its
/// description.
fn fetch_images(agent: &ureq::Agent, page: &mut Page) {
    const MOST: usize = 6;
    const EACH: usize = 512 << 10;
    const ALL: usize = 2 << 20;
    let mut wanted = Vec::new();
    for (i, block) in page.blocks.iter().enumerate() {
        if let Block::Image { url, data: None, .. } = block {
            if readable_url(url) && wanted.len() < MOST {
                wanted.push((i, url.clone()));
            }
        }
    }
    let mut total = 0;
    for (i, url) in wanted {
        let Some(bytes) = fetch_image(agent, &url, EACH).filter(|b| total + b.len() <= ALL) else { continue };
        total += bytes.len();
        if let Some(Block::Image { data, .. }) = page.blocks.get_mut(i) {
            *data = Some(bytes);
        }
    }
}

fn fetch_image(agent: &ureq::Agent, url: &str, limit: usize) -> Option<Vec<u8>> {
    let response = agent
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "image/*")
        .call()
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let kind = header(&response, "content-type").unwrap_or_default().to_ascii_lowercase();
    if !(kind.starts_with("image/") || kind.is_empty()) {
        return None;
    }
    let mut reader = response.into_body().into_with_config().limit(u64::MAX).reader();
    let mut bytes = Vec::new();
    (&mut reader).take(limit as u64 + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() <= limit).then_some(bytes)
}


/// Save the file at `url` to `path`, telling how far it has come. Returns
/// the bytes written. A save that stops leaves no part behind.
pub fn save(
    agent: &ureq::Agent,
    url: &str,
    path: &Path,
    stop: &AtomicBool,
    progress: &mut dyn FnMut(Saving),
) -> Result<u64> {
    if !readable_url(url) {
        bail!("{url} is not an http or https address");
    }
    let dir = path.parent().context("a download has a folder to go in")?;
    fs::create_dir_all(dir).with_context(|| format!("make {}", dir.display()))?;
    let part = partial(path);
    let response = agent
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "*/*")
        .call()
        .with_context(|| format!("start downloading {url}"))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        bail!("the server answered {status} for {url}");
    }
    let total = crate::download::content_length(response.headers());
    if let Some(total) = total {
        let room = download::free(dir)?;
        if total > room {
            bail!("{total} bytes are needed, and {room} are free");
        }
    }
    let mut response = response;
    let mut body = response.body_mut().with_config().limit(u64::MAX).reader();
    let mut file = File::create(&part).with_context(|| format!("write {}", part.display()))?;
    let mut buf = vec![0u8; 64 << 10];
    let mut done = 0u64;
    progress(Saving { done, total });
    loop {
        if stop.load(Ordering::Relaxed) {
            drop(file);
            let _ = fs::remove_file(&part);
            bail!(crate::Stopped);
        }
        let n = body.read(&mut buf).context("read the download")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).with_context(|| format!("write {}", part.display()))?;
        done += n as u64;
        progress(Saving { done, total });
    }
    file.sync_all().with_context(|| format!("write {} through", part.display()))?;
    if let Some(total) = total {
        if done != total {
            drop(file);
            let _ = fs::remove_file(&part);
            bail!("the server sent {done} of {total} bytes");
        }
    }
    fs::rename(&part, path).with_context(|| format!("name {} {}", part.display(), path.display()))?;
    Ok(done)
}

/// Where a download waits until it is whole.
pub fn partial(path: &Path) -> PathBuf {
    let mut name = path.file_name().expect("a download has a name").to_os_string();
    name.push(".part");
    path.with_file_name(name)
}

fn header(response: &ureq::http::Response<ureq::Body>, name: &str) -> Option<String> {
    response.headers().get(name)?.to_str().ok().map(str::to_string)
}

/// The name a server gives a file in `Content-Disposition`.
fn disposition_name(response: &ureq::http::Response<ureq::Body>) -> Option<String> {
    let value = header(response, "content-disposition")?;
    let mut fallback = None;
    for part in value.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("filename*=") {
            let rest = rest.trim_matches('"');
            let encoded = rest.split("''").nth(1).unwrap_or(rest);
            let name = percent_decode(encoded);
            if !name.is_empty() {
                return Some(sanitize_name(&name));
            }
        }
        if let Some(rest) = part.strip_prefix("filename=") {
            let name = rest.trim().trim_matches('"');
            if !name.is_empty() {
                fallback = Some(sanitize_name(&name));
            }
        }
    }
    fallback
}

/// A name a filesystem can take: no separators or controls, and not empty
/// or a dot.
fn sanitize_name(name: &str) -> String {
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_control() || matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
            out.push('_');
        } else {
            out.push(ch);
        }
    }
    let out = out.trim_matches([' ', '.']).to_string();
    let mut chars: String = out.chars().take(120).collect();
    if chars.is_empty() || chars == "." || chars == ".." {
        chars = String::from("download");
    }
    chars
}

/// A page for what a server gives as plain text: blank lines divide
/// paragraphs, and an address on a line of its own is a link.
fn plain_page(url: &str, text: &str, truncated: bool) -> Page {
    let mut blocks = Vec::new();
    for para in text.split("\n\n").take(MAX_BLOCKS) {
        let para = para.trim();
        if para.is_empty() {
            continue;
        }
        if readable_url(para) && !para.contains(char::is_whitespace) {
            blocks.push(Block::Link {
                label: para.to_string(),
                url: para.to_string(),
                download: download_name(para).is_some(),
                style: Style::default(),
            });
        } else {
            blocks.push(Block::Paragraph {
                text: para.chars().take(MAX_BLOCK_CHARS).collect(),
                style: Style::default(),
            });
        }
    }
    let title = blocks
        .iter()
        .find_map(|b| match b {
            Block::Heading { text, .. } | Block::Paragraph { text, .. } => Some(text.chars().take(80).collect()),
            _ => None,
        })
        .unwrap_or_else(|| host_of(url));
    Page {
        url: url.to_string(),
        title,
        site: host_of(url),
        description: None,
        blocks,
        forms: Vec::new(),
        cookies: Vec::new(),
        truncated,
    }
}

/// A page for an RSS answer, as Bing's search gives: each item's title is a
/// link, and its description follows as a short paragraph.
fn rss_page(url: &str, text: &str, truncated: bool) -> Page {
    let mut blocks = Vec::new();
    let mut rest = text;
    let mut n = 0;
    while let Some(at) = find_ci(rest, "<item") {
        let after = &rest[at..];
        let Some(open) = after.find('>') else { break };
        let body = &after[open + 1..];
        let Some(end) = find_ci(body, "</item>") else { break };
        let item = &body[..end];
        rest = &body[end + 7..];
        let title = tag_text(item, "title").map(|t| clean_text(&t)).unwrap_or_default();
        let link = tag_text(item, "link").map(|t| clean_text(&t)).unwrap_or_default();
        if title.is_empty() || link.is_empty() {
            continue;
        }
        let Some(link) = absolute(url, &link) else { continue };
        blocks.push(Block::Link {
            label: title,
            url: link,
            download: false,
            style: Style { bold: true, size: Some(17), ..Style::default() },
        });
        if let Some(description) = tag_text(item, "description").map(|t| clean_text(&t)).filter(|d| !d.is_empty()) {
            let description: String = description.chars().take(360).collect();
            blocks.push(Block::Paragraph { text: description, style: Style { size: Some(14), ..Style::default() } });
        }
        n += 1;
        if n >= 30 || blocks.len() >= MAX_BLOCKS {
            break;
        }
    }
    let mut page = Page {
        url: url.to_string(),
        title: if n > 0 { "Bing 搜索".to_string() } else { host_of(url) },
        site: host_of(url),
        description: None,
        blocks,
        forms: Vec::new(),
        cookies: Vec::new(),
        truncated: truncated || n >= 30,
    };
    if page.blocks.is_empty() {
        page.blocks.push(Block::Paragraph {
            text: "Bing 没有给出可读的结果，换个关键词试试。".into(),
            style: Style::default(),
        });
    }
    page
}

fn tag_text(item: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let at = find_ci(item, &open)?;
    let after = &item[at + open.len()..];
    let start = after.find('>')? + 1;
    let body = &after[start..];
    let close = find_ci(body, &format!("</{tag}>"))?;
    Some(body[..close].to_string())
}

fn clean_text(raw: &str) -> String {
    let raw = raw.trim().trim_start_matches("<![CDATA[").trim_end_matches("]]>");
    let mut text = String::with_capacity(raw.len());
    let mut in_tag = false;
    for ch in raw.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => text.push(ch),
            _ => {}
        }
    }
    collapse(&entities(&text))
}

/// The host of `url`, without `www.`.
pub fn host_of(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    host.strip_prefix("www.").unwrap_or(host).to_string()
}

/// A downloaded file's folder under internal storage, as the pages write
/// it.
pub const FOLDER: &str = "Download/Skyward";

/// Read an HTML page's text, headings, lists and links.
pub fn parse(url: &str, html: &str, truncated: bool) -> Page {
    let content = body_of(html);
    let mut p = Parser::new(url);
    p.rules = css_rules(html);
    if let Some(href) = base_href(html) {
        if let Some(base) = absolute(url, &href) {
            p.base = base;
        }
    }
    let mut rest = content;
    while !rest.is_empty() {
        if let Some(tag) = p.skip_tag() {
            let close = format!("</{tag}");
            match find_ci(rest, &close) {
                Some(at) => {
                    let after = &rest[at + close.len()..];
                    let end = after.find('>').map(|i| i + 1).unwrap_or(after.len());
                    rest = &after[end..];
                }
                None => rest = "",
            }
            p.stop_skipping();
            continue;
        }
        if p.too_long() {
            break;
        }
        let Some(at) = rest.find('<') else {
            p.text(rest);
            break;
        };
        p.text(&rest[..at]);
        if rest[at..].starts_with("<!--") {
            match rest[at + 4..].find("-->") {
                Some(end) => rest = &rest[at + 4 + end + 3..],
                None => break,
            }
            continue;
        }
        let Some(end) = rest[at..].find('>') else {
            break;
        };
        let tag = rest[at + 1..at + end].to_string();
        rest = &rest[at + end + 1..];
        p.tag(&tag);
    }
    p.finish(truncated)
}

/// The address a page's `<base href>` gives its relative links, where it
/// gives one.
fn base_href(html: &str) -> Option<String> {
    let at = find_ci(html, "<base")?;
    let after = &html[at + 5..];
    if !matches!(after.chars().next(), Some('>' | ' ' | '\t' | '\n' | '\r' | '/')) {
        return None;
    }
    let end = html[at..].find('>')?;
    let tag = &html[at + 1..at + end];
    attr(&attributes(tag), "href")
}

/// The part of `html` a page keeps its own text in: from its `<main>` or
/// `<article>` on, where it has one, and the whole page otherwise. What is
/// outside is the site's menus, which a reader does not need.
fn body_of(html: &str) -> &str {
    for tag in ["main", "article"] {
        let open = format!("<{tag}");
        let Some(at) = find_ci(html, &open) else { continue };
        let after = &html[at + open.len()..];
        let next = after.chars().next();
        if !matches!(next, Some('>' | ' ' | '\t' | '\n' | '\r')) {
            continue;
        }
        let Some(start) = after.find('>').map(|i| at + open.len() + i + 1) else { continue };
        let close = format!("</{tag}");
        let end = find_ci(&html[start..], &close).map_or(html.len(), |i| start + i);
        let body = &html[start..end];
        if !body.trim().is_empty() {
            return body;
        }
    }
    html
}

struct Parser {
    base: String,
    title: String,
    description: Option<String>,
    blocks: Vec<Block>,
    text: String,
    /// What the text being read is part of.
    mode: Mode,
    /// The `<title>` is being read.
    title_tag: bool,
    /// The link being read, its address and its words.
    link: Option<(String, String)>,
    /// The tag whose content is skipped, such as `script`.
    skip: Option<String>,
    /// How the text being read is set.
    style: Style,
    /// The page's own stylesheet.
    rules: Vec<CssRule>,
    /// The forms read, and the one being read.
    forms: Vec<Form>,
    form: Option<usize>,
    /// The last block pushed, to join what a page split around inline tags.
    truncated: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Para,
    Heading(u8),
    Bullet,
    Quote,
    Code,
}

impl Parser {
    fn new(url: &str) -> Self {
        Parser {
            base: url.to_string(),
            title: String::new(),
            description: None,
            blocks: Vec::new(),
            text: String::new(),
            mode: Mode::Para,
            title_tag: false,
            link: None,
            skip: None,
            style: Style::default(),
            rules: Vec::new(),
            forms: Vec::new(),
            form: None,
            truncated: false,
        }
    }

    fn skip_tag(&self) -> Option<&str> {
        self.skip.as_deref()
    }

    fn stop_skipping(&mut self) {
        self.skip = None;
    }

    fn too_long(&self) -> bool {
        self.truncated || self.blocks.len() >= MAX_BLOCKS
    }

    fn text(&mut self, raw: &str) {
        let decoded = entities(raw);
        if self.title_tag {
            self.title.push_str(&decoded);
            return;
        }
        if self.mode == Mode::Code {
            self.text.push_str(&decoded);
            return;
        }
        let mut space = false;
        let target = match &mut self.link {
            Some((_, label)) => label,
            None => &mut self.text,
        };
        for ch in decoded.chars() {
            if ch.is_whitespace() {
                space = true;
            } else {
                if space && !target.is_empty() && !target.ends_with(' ') {
                    target.push(' ');
                }
                space = false;
                target.push(ch);
            }
        }
        if space && !target.is_empty() && !target.ends_with(' ') {
            target.push(' ');
        }
    }

    fn tag(&mut self, raw: &str) {
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with('!') || raw.starts_with('?') {
            return;
        }
        let closing = raw.starts_with('/');
        let body = if closing { raw[1..].trim_start() } else { raw };
        let self_closing = body.ends_with('/');
        let body = body.trim_end_matches('/').trim_end();
        let name_end = body.find(char::is_whitespace).unwrap_or(body.len());
        let name = body[..name_end].to_ascii_lowercase();
        if name.is_empty() {
            return;
        }
        let attrs = attributes(&body[name_end..]);
        if closing {
            self.close(&name);
        } else {
            self.open(&name, &attrs, self_closing);
        }
    }

    fn open(&mut self, name: &str, attrs: &[(String, String)], self_closing: bool) {
        if self_closing && !matches!(name, "br" | "img" | "hr" | "meta" | "base" | "input") {
            return;
        }
        match name {
            "script" | "style" | "noscript" | "template" | "svg" | "iframe" | "canvas" | "nav" | "footer" | "aside" | "select"
            | "option" | "textarea" => {
                self.skip = Some(name.to_string());
            }
            "title" => self.title_tag = true,
            "meta" => {
                let key = attr(attrs, "name").or_else(|| attr(attrs, "property")).unwrap_or_default();
                if key.eq_ignore_ascii_case("description") || key.eq_ignore_ascii_case("og:description") {
                    if let Some(content) = attr(attrs, "content") {
                        if !content.trim().is_empty() {
                            self.description = Some(content.trim().to_string());
                        }
                    }
                }
            }
            "base" => {
                if let Some(href) = attr(attrs, "href") {
                    if let Some(base) = absolute(&self.base, &href) {
                        self.base = base;
                    }
                }
            }
            "br" => self.text(" "),
            "p" | "div" | "section" | "article" | "main" | "header" | "form" | "fieldset"
            | "figure" | "figcaption" | "table" | "thead" | "tbody" | "tr" | "dl" | "dt" | "dd" | "address" | "details" | "summary" => {
                self.flush();
                self.style = self.style_for(name, attrs);
                self.mode = Mode::Para;
                if name == "form" {
                    self.form_start(attrs);
                }
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.flush();
                self.style = self.style_for(name, attrs);
                self.style.bold = true;
                self.mode = Mode::Heading(name.as_bytes()[1] - b'0');
            }
            "ul" | "ol" => {
                self.flush();
                self.mode = Mode::Para;
            }
            "li" => {
                self.flush();
                self.style = self.style_for(name, attrs);
                self.mode = Mode::Bullet;
            }
            "blockquote" => {
                self.flush();
                self.style = self.style_for(name, attrs);
                self.mode = Mode::Quote;
            }
            "pre" => {
                self.flush();
                self.style = self.style_for(name, attrs);
                self.mode = Mode::Code;
            }
            "hr" => {
                self.flush();
                if self.blocks.len() < MAX_BLOCKS {
                    self.blocks.push(Block::Rule);
                }
            }
            "b" | "strong" => self.style.bold = true,
            "i" | "em" => self.style.italic = true,
            "u" | "ins" => self.style.underline = true,
            "center" => self.style.align = Align::Center,
            "font" => {
                if let Some(color) = attr(attrs, "color").and_then(|c| color(&c)) {
                    self.style.color = Some(color);
                }
            }
            "img" => {
                let style = self.style_for(name, attrs);
                if let Some(src) = attr(attrs, "src") {
                    if let Some(url) = absolute(&self.base, &src) {
                        let alt = attr(attrs, "alt").unwrap_or_default().trim().to_string();
                        let width = attr(attrs, "width").and_then(|w| w.parse().ok()).unwrap_or(0);
                        let height = attr(attrs, "height").and_then(|h| h.parse().ok()).unwrap_or(0);
                        if self.blocks.len() < MAX_BLOCKS {
                            self.blocks.push(Block::Image { alt, url, data: None, width, height, style });
                        }
                    }
                }
            }
            "input" => self.input(attrs),
            "button" => {
                if let Some(form) = self.form {
                    let label = attr(attrs, "value")
                        .or_else(|| attr(attrs, "aria-label"))
                        .unwrap_or_else(|| "Submit".to_string());
                    if let Some(f) = self.forms.get_mut(form) {
                        f.fields.push(FormField {
                            name: String::new(),
                            label,
                            kind: FieldKind::Submit,
                            value: String::new(),
                            placeholder: String::new(),
                        });
                    }
                }
            }
            "a" => {
                if let Some(href) = attr(attrs, "href") {
                    self.flush();
                    self.link = Some((href, String::new()));
                }
            }
            _ => {
                let _ = (attrs, self_closing);
            }
        }
    }

    fn close(&mut self, name: &str) {
        match name {
            "title" => self.title_tag = false,
            "a" => {
                if let Some((href, label)) = self.link.take() {
                    let label = collapse(&label);
                    if !label.is_empty() {
                        if let Some(url) = absolute(&self.base, &href) {
                            if url != self.base {
                                let download = download_name(&url).is_some();
                                self.blocks.push(Block::Link { label, url, download, style: self.style });
                            }
                        }
                    }
                }
            }
            "form" => self.form_end(),
            "p" | "div" | "section" | "article" | "main" | "header"
            | "figure" | "figcaption" | "table" | "thead" | "tbody" | "tr" | "dl" | "dt" | "dd" | "address" | "details" | "summary"
            | "li" | "blockquote" | "pre" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "ul" | "ol" => {
                self.flush();
                self.mode = Mode::Para;
            }
            _ => {}
        }
    }

    fn form_start(&mut self, attrs: &[(String, String)]) {
        if self.form.is_some() {
            return;
        }
        let action = attr(attrs, "action").unwrap_or_default();
        let post = attr(attrs, "method").is_some_and(|m| m.eq_ignore_ascii_case("post"));
        self.forms.push(Form { action, post, fields: Vec::new() });
        self.form = Some(self.forms.len() - 1);
    }

    fn form_end(&mut self) {
        let Some(i) = self.form.take() else { return };
        self.flush();
        if let Some(form) = self.forms.get_mut(i) {
            form.action = absolute(&self.base, &form.action).unwrap_or_else(|| self.base.clone());
        }
        if self.blocks.len() < MAX_BLOCKS {
            self.blocks.push(Block::Form(i));
        }
    }

    /// How an element is set: the page's stylesheet rules that name it,
    /// then its own `style`.
    fn style_for(&self, name: &str, attrs: &[(String, String)]) -> Style {
        let class_attr = attr(attrs, "class").unwrap_or_default();
        let id_attr = attr(attrs, "id").unwrap_or_default();
        let classes: Vec<&str> = class_attr.split_whitespace().collect();
        let mut style = Style::default();
        for rule in &self.rules {
            let tag = rule.tag.as_deref().is_none_or(|tag| tag == name);
            let class = rule.class.as_deref().is_none_or(|class| classes.contains(&class));
            let id = rule.id.as_deref().is_none_or(|id| id == id_attr);
            if tag && class && id {
                style.merge(rule.style);
            }
        }
        style.merge(inline_style(attrs));
        style
    }

    fn input(&mut self, attrs: &[(String, String)]) {        let Some(i) = self.form else { return };
        let kind = attr(attrs, "type").unwrap_or_else(|| "text".into()).to_ascii_lowercase();
        let name = attr(attrs, "name").unwrap_or_default();
        let label = attr(attrs, "aria-label")
            .or_else(|| attr(attrs, "placeholder"))
            .or_else(|| attr(attrs, "name"))
            .or_else(|| attr(attrs, "value"))
            .unwrap_or_default();
        let field = match kind.as_str() {
            "hidden" => FormField {
                name,
                label: String::new(),
                kind: FieldKind::Hidden,
                value: attr(attrs, "value").unwrap_or_default(),
                placeholder: String::new(),
            },
            "submit" | "button" => FormField {
                name,
                label: if label.is_empty() { "Submit".into() } else { label },
                kind: FieldKind::Submit,
                value: String::new(),
                placeholder: String::new(),
            },
            "password" => FormField { name, label, kind: FieldKind::Password, value: String::new(), placeholder: String::new() },
            "checkbox" => FormField {
                name,
                label,
                kind: FieldKind::Checkbox,
                value: attr(attrs, "value").unwrap_or_else(|| "on".into()),
                placeholder: String::new(),
            },
            "radio" => FormField {
                name,
                label,
                kind: FieldKind::Radio,
                value: attr(attrs, "value").unwrap_or_default(),
                placeholder: String::new(),
            },
            _ => FormField {
                name,
                label,
                kind: FieldKind::Text,
                value: attr(attrs, "value").unwrap_or_default(),
                placeholder: attr(attrs, "placeholder").unwrap_or_default(),
            },
        };
        if let Some(form) = self.forms.get_mut(i) {
            form.fields.push(field);
        }
    }

    /// Put what has been read into the block being read.
    fn flush(&mut self) {
        if let Some((href, label)) = self.link.take() {
            let label = collapse(&label);
            if !label.is_empty() {
                if let Some(url) = absolute(&self.base, &href) {
                    if url != self.base {
                        let download = download_name(&url).is_some();
                        self.blocks.push(Block::Link { label, url, download, style: self.style });
                    }
                }
            }
        }
        let text = collapse(&self.text);
        self.text.clear();
        if text.is_empty() {
            return;
        }
        if self.blocks.len() >= MAX_BLOCKS {
            self.truncated = true;
            return;
        }
        let text: String = text.chars().take(MAX_BLOCK_CHARS).collect();
        let style = self.style;
        let block = match self.mode {
            Mode::Para => Block::Paragraph { text, style },
            Mode::Heading(level) => Block::Heading { level, text, style },
            Mode::Bullet => Block::Bullet { text, style },
            Mode::Quote => Block::Quote { text, style },
            Mode::Code => Block::Code { text, style },
        };
        self.blocks.push(block);
    }

    fn finish(&mut self, cut: bool) -> Page {
        self.flush();
        let mut title = collapse(&self.title);
        if title.is_empty() {
            title = self
                .blocks
                .iter()
                .find_map(|b| match b {
                    Block::Heading { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default();
        }
        if title.is_empty() {
            title = host_of(&self.base);
        }
        Page {
            url: self.base.clone(),
            title,
            site: host_of(&self.base),
            description: self.description.take(),
            blocks: std::mem::take(&mut self.blocks),
            forms: std::mem::take(&mut self.forms),
            cookies: Vec::new(),
            truncated: cut || self.truncated,
        }
    }
}

fn collapse(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            space = true;
        } else {
            if space && !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
            }
            space = false;
            out.push(ch);
        }
    }
    out.trim().to_string()
}

fn attr(attrs: &[(String, String)], name: &str) -> Option<String> {
    attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
}

/// How an element's own `style` says to set it: the few properties a
/// reader can use, with the rest left to the phone's own idea of a page.
fn inline_style(attrs: &[(String, String)]) -> Style {
    let mut style = attr(attrs, "style").map(|raw| declarations(&raw)).unwrap_or_default();
    if let Some(raw) = attr(attrs, "align") {
        style.align = match raw.to_ascii_lowercase().as_str() {
            "center" => Align::Center,
            "right" => Align::Right,
            _ => Align::Left,
        };
    }
    style
}

fn declarations(raw: &str) -> Style {
    let mut style = Style::default();
    for part in raw.split(';') {
        let Some((name, value)) = part.split_once(':') else { continue };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_ascii_lowercase();
        match name.as_str() {
            "color" => style.color = color(&value),
            "background" | "background-color" => style.background = color(&value),
            "font-weight" => style.bold = value == "bold" || value.parse::<u16>().is_ok_and(|n| n >= 600),
            "font-style" => style.italic = value == "italic" || value == "oblique",
            "text-decoration" | "text-decoration-line" => style.underline = value.contains("underline"),
            "text-align" => {
                style.align = match value.as_str() {
                    "center" => Align::Center,
                    "right" | "end" => Align::Right,
                    _ => Align::Left,
                }
            }
            "font-size" => style.size = value.trim_end_matches("px").parse().ok(),
            _ => {}
        }
    }
    style
}

/// The rules in a page's `<style>` elements, as far as a reader can use
/// them: a tag, class or id, and no descendant selectors.
fn css_rules(html: &str) -> Vec<CssRule> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(at) = find_ci(rest, "<style") {
        let after = &rest[at + 6..];
        let Some(end) = after.find('>') else { break };
        let css_start = &after[end + 1..];
        let Some(close) = find_ci(css_start, "</style") else { break };
        let css = &css_start[..close];
        for block in css.split('}') {
            let Some((selector, body)) = block.split_once('{') else { continue };
            let body = body.split("/*").next().unwrap_or(body);
            let style = declarations(body);
            for selector in selector.split(',') {
                let selector = selector.trim();
                if selector.is_empty() || selector.contains([' ', '>', '+', '~', ':', '[', '*']) {
                    continue;
                }
                let (tag, class, id) = if let Some(class) = selector.strip_prefix('.') {
                    (None, Some(class.to_string()), None)
                } else if let Some(id) = selector.strip_prefix('#') {
                    (None, None, Some(id.to_string()))
                } else {
                    (Some(selector.to_ascii_lowercase()), None, None)
                };
                out.push(CssRule { tag, class, id, style });
            }
        }
        rest = &css_start[close + 7..];
    }
    out
}

/// A colour as CSS writes it, where it is one a recovery can show.
fn color(raw: &str) -> Option<u32> {
    let raw = raw.trim();
    if let Some(hex) = raw.strip_prefix('#') {
        let digits: String = hex.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        return match digits.len() {
            3 => {
                let r = u8::from_str_radix(&digits[0..1], 16).ok()? * 17;
                let g = u8::from_str_radix(&digits[1..2], 16).ok()? * 17;
                let b = u8::from_str_radix(&digits[2..3], 16).ok()? * 17;
                Some(u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b))
            }
            6 => u32::from_str_radix(&digits, 16).ok(),
            _ => None,
        };
    }
    Some(match raw {
        "black" => 0x000000,
        "white" => 0xffffff,
        "red" => 0xcc3333,
        "green" => 0x338833,
        "blue" => 0x3366cc,
        "gray" | "grey" => 0x888888,
        "orange" => 0xcc7733,
        "yellow" => 0xcccc33,
        "purple" => 0x7733cc,
        _ => return None,
    })
}


/// The attributes of a tag, in lower case, their values decoded.
fn attributes(raw: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' && bytes[i] != b'/' {
            i += 1;
        }
        if start == i {
            i += 1;
            continue;
        }
        let key = raw[start..i].to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let quote = bytes[i];
                i += 1;
                let at = i;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                value = entities(&raw[at..i]);
                if i < bytes.len() {
                    i += 1;
                }
            } else {
                let at = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
                    i += 1;
                }
                value = entities(&raw[at..i]);
            }
        }
        out.push((key, value));
    }
    out
}

/// Decode the entities HTML writes for characters.
fn entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest[..rest.len().min(34)].find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let name = &rest[1..end];
        if let Some(ch) = entity(name) {
            out.push(ch);
            rest = &rest[end + 1..];
        } else {
            out.push('&');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    if let Some(hex) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
        return u32::from_str_radix(hex, 16).ok().and_then(char::from_u32);
    }
    if let Some(dec) = name.strip_prefix('#') {
        return dec.parse::<u32>().ok().and_then(char::from_u32);
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "ensp" => ' ',
        "emsp" => ' ',
        "thinsp" => ' ',
        "zwnj" => '\u{200c}',
        "zwj" => '\u{200d}',
        "ndash" => '–',
        "mdash" => '—',
        "hellip" => '…',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "laquo" => '«',
        "raquo" => '»',
        "middot" => '·',
        "bull" => '•',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "times" => '×',
        "divide" => '÷',
        "plusmn" => '±',
        "deg" => '°',
        "micro" => 'µ',
        "euro" => '€',
        "pound" => '£',
        "yen" => '¥',
        "cent" => '¢',
        "sect" => '§',
        "para" => '¶',
        "dagger" => '†',
        "Dagger" => '‡',
        "permil" => '‰',
        "prime" => '′',
        "Prime" => '″',
        "larr" => '←',
        "uarr" => '↑',
        "rarr" => '→',
        "darr" => '↓',
        "harr" => '↔',
        "minus" => '−',
        "frac12" => '½',
        "frac14" => '¼',
        "frac34" => '¾',
        "sup2" => '²',
        "sup3" => '³',
        "alpha" => 'α',
        "beta" => 'β',
        "gamma" => 'γ',
        "pi" => 'π',
        "infin" => '∞',
        "ne" => '≠',
        "le" => '≤',
        "ge" => '≥',
        _ => return None,
    })
}

/// The address `href` names from the page at `base`, with no fragment.
fn absolute(base: &str, href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    let lower = href.to_ascii_lowercase();
    if lower.starts_with("javascript:") || lower.starts_with("mailto:") || lower.starts_with("tel:") || lower.starts_with("data:") {
        return None;
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Some(href.split('#').next().unwrap_or(href).to_string());
    }
    let (scheme, rest) = base.split_once("://")?;
    if let Some(rest) = href.strip_prefix("//") {
        return Some(format!("{scheme}://{}", rest.split('#').next().unwrap_or(rest)));
    }
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let path = if authority_end < rest.len() { &rest[authority_end..] } else { "/" };
    let path = path.split(['?', '#']).next().unwrap_or("/");
    let href = href.split('#').next().unwrap_or(href);
    if href.starts_with('/') {
        return Some(format!("{scheme}://{authority}{href}"));
    }
    if href.starts_with('?') {
        return Some(format!("{scheme}://{authority}{path}{href}"));
    }
    let mut parts: Vec<&str> = path.split('/').collect();
    parts.pop();
    for part in href.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.len() > 1 {
                    parts.pop();
                }
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    Some(format!("{scheme}://{authority}{}", if joined.starts_with('/') { joined } else { format!("/{joined}") }))
}

fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    let hay = haystack.as_bytes();
    let needle = needle.as_bytes();
    if needle.is_empty() {
        return Some(0);
    }
    hay.windows(needle.len()).position(|window| window.eq_ignore_ascii_case(needle))
}

fn percent_encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(byte as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| -> Option<u8> {
                match b {
                    b'0'..=b'9' => Some(b - b'0'),
                    b'a'..=b'f' => Some(b - b'a' + 10),
                    b'A'..=b'F' => Some(b - b'A' + 10),
                    _ => None,
                }
            };
            if let (Some(a), Some(b)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(a * 16 + b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_is_taken_as_one_and_words_are_searched() {
        assert_eq!(address("https://example.com/a"), "https://example.com/a");
        assert_eq!(address("example.com/a"), "https://example.com/a");
        assert_eq!(address("//example.com"), "https://example.com");
        assert!(address("skyward recovery").starts_with("https://www.bing.com/search?format=rss"));
    }

    #[test]
    fn relative_links_lose_their_fragment_and_go_up() {
        let base = "https://example.com/a/b/page.html?q=1";
        assert_eq!(absolute(base, "c.html").as_deref(), Some("https://example.com/a/b/c.html"));
        assert_eq!(absolute(base, "../d.html").as_deref(), Some("https://example.com/a/d.html"));
        assert_eq!(absolute(base, "/e.html").as_deref(), Some("https://example.com/e.html"));
        assert_eq!(absolute(base, "?page=2").as_deref(), Some("https://example.com/a/b/page.html?page=2"));
        assert_eq!(absolute(base, "#top"), None);
        assert_eq!(absolute(base, "javascript:void(0)"), None);
    }

    #[test]
    fn a_page_is_read_as_headings_paragraphs_and_links() {
        let html = r#"
            <html><head><title>A test &amp; more</title>
            <meta name="description" content="What it is"></head>
            <body><script>var x = "<p>not text</p>";</script>
            <h1>Hello</h1><p>One <b>bold</b> sentence with &lt;tags&gt;.</p>
            <ul><li>First</li><li>Second</li></ul>
            <a href="/download/rom.zip">Download the ROM</a>
            <a href="other.html">Other page</a></body></html>"#;
        let page = parse("https://example.com/a/", html, false);
        assert_eq!(page.title, "A test & more");
        assert_eq!(page.description.as_deref(), Some("What it is"));
        assert!(page.blocks.iter().any(|b| matches!(b, Block::Heading { level: 1, text, .. } if text == "Hello")));
        assert!(page.blocks.iter().any(|b| matches!(b, Block::Paragraph { text, .. } if text == "One bold sentence with <tags>.")));
        assert!(page.blocks.iter().any(|b| matches!(b, Block::Bullet { text, .. } if text == "First")));
        assert!(page.blocks.contains(&Block::Link { label: "Download the ROM".into(), url: "https://example.com/download/rom.zip".into(), download: true, style: Style::default() }));
        assert!(page.blocks.contains(&Block::Link { label: "Other page".into(), url: "https://example.com/a/other.html".into(), download: false, style: Style::default() }));
    }

    #[test]
    fn a_base_address_is_kept_for_links_outside_the_main_text() {
        let html = "<head><base href=\"https://cdn.example.com/files/\"></head><main><a href=\"rom.zip\">ROM</a></main>";
        let page = parse("https://example.com/releases/", html, false);
        assert!(page.blocks.contains(&Block::Link {
            label: "ROM".into(),
            url: "https://cdn.example.com/files/rom.zip".into(),
            download: true,
            style: Style::default(),
        }));
    }

    #[test]
    fn scripts_styles_and_comments_show_nothing() {
        let html = "<style>p{color:red}</style><!-- hidden --><p>Shown</p><script>alert('no')</script>";
        let page = parse("https://example.com/", html, false);
        let text = page
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Heading { text, .. }
                | Block::Paragraph { text, .. }
                | Block::Bullet { text, .. }
                | Block::Quote { text, .. }
                | Block::Code { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("Shown"));
        assert!(!text.contains("color"));
        assert!(!text.contains("hidden"));
        assert!(!text.contains("alert"));
    }

    #[test]
    fn bing_s_rss_answer_is_read_as_results() {
        let rss = r#"<?xml version="1.0"?><rss><channel><title>Bing</title>
            <item><title>Skyward recovery</title><link>https://example.com/skyward</link><description>Rust recovery</description></item>
            <item><title>Another result</title><link>https://example.com/two</link><description>More</description></item>
        </channel></rss>"#;
        let page = rss_page("https://www.bing.com/search?format=rss&q=skyward", rss, false);
        assert!(page.blocks.iter().any(|b| matches!(b, Block::Link { label, .. } if label == "Skyward recovery")));
        assert!(page.blocks.iter().any(|b| matches!(b, Block::Paragraph { text, .. } if text == "Rust recovery")));
        assert!(page.title.contains("Bing"));
    }


    #[test]
    fn a_page_s_own_css_and_forms_are_kept() {
        let html = "<style>p{color:#cc3333;font-weight:bold} .lead{font-size:22px}</style>\
            <p class=\"lead\">Warm</p>\
            <form action=\"/search\" method=\"post\"><input name=\"q\" value=\"skyward\"><input type=\"submit\" value=\"Go\"></form>";
        let page = parse("https://example.com/", html, false);
        let warm = page
            .blocks
            .iter()
            .find_map(|b| match b {
                Block::Paragraph { text, style } if text == "Warm" => Some(style),
                _ => None,
            })
            .expect("the paragraph is read");
        assert_eq!(warm.color, Some(0xcc3333));
        assert!(warm.bold);
        assert_eq!(page.forms.len(), 1);
        assert_eq!(page.forms[0].action, "https://example.com/search");
        assert!(page.forms[0].post);
        assert_eq!(page.forms[0].fields[0].name, "q");
        assert_eq!(page.forms[0].fields[1].kind, FieldKind::Submit);
        assert!(page.blocks.iter().any(|b| matches!(b, Block::Form(0))));
    }

    #[test]
    fn cookies_are_kept_for_the_host_and_path() {
        let mut headers = ureq::http::HeaderMap::new();
        headers.append("set-cookie", "a=1; Path=/; Domain=example.com".parse().unwrap());
        headers.append("set-cookie", "b=2; Path=/deep".parse().unwrap());
        let cookies = read_cookies(&headers, "https://www.example.com/deep/page");
        assert_eq!(cookies.len(), 2);
        assert_eq!(cookies[0].domain, "example.com");
        let pairs: Vec<(String, String, String, String)> =
            cookies.iter().map(|c| (c.name.clone(), c.value.clone(), c.domain.clone(), c.path.clone())).collect();
        assert_eq!(cookie_header(&pairs, "https://www.example.com/deep/page").as_deref(), Some("a=1; b=2"));
        assert_eq!(cookie_header(&pairs, "https://www.example.com/other"), Some("a=1".into()));
        assert_eq!(cookie_header(&pairs, "https://elsewhere.com/"), None);
    }


    #[test]
    fn a_download_is_named_safely() {
        assert_eq!(file_name("https://example.com/a/My%20File.zip?x=1"), "My File.zip");
        assert_eq!(file_name("https://example.com/a/..%2Fetc%2Fpasswd"), "passwd");
        assert!(installable("rom.zip") && installable("boot.img") && !installable("a.apk"));
        assert!(download_name("https://example.com/rom.zip").is_some());
        assert!(download_name("https://example.com/index.html").is_none());
    }

    #[test]
    fn entities_become_their_characters() {
        assert_eq!(entities("a&amp;b &#65; &#x4e2d; &unknown;"), "a&b A 中 &unknown;");
    }

    fn serve(content_type: &str, body: &[u8]) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let head = format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        let bytes = body.to_vec();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(&bytes).unwrap();
        });
        format!("http://127.0.0.1:{port}/")
    }

    #[test]
    fn a_served_page_is_read_and_a_served_file_is_offered() {
        let url = serve("text/html; charset=utf-8", b"<title>Local</title><p>Hello <a href='/next'>next</a></p>");
        let agent = ureq::Agent::new_with_defaults();
        let Opened::Page(page) = open(&agent, &url).unwrap() else { panic!("a page was served") };
        assert_eq!(page.title, "Local");
        assert!(page.blocks.iter().any(|b| matches!(b, Block::Paragraph { text, .. } if text == "Hello")));
        assert!(page.blocks.iter().any(|b| matches!(b, Block::Link { url, .. } if url.ends_with("/next"))));

        let url = serve("application/zip", b"PK\x03\x04zip");
        let Opened::File(file) = open(&agent, &url).unwrap() else { panic!("a file was served") };
        assert_eq!(file.size, Some(7));
        assert_eq!(file.kind, "application/zip");
    }

    #[test]
    fn a_served_file_is_saved_where_it_was_asked_to_go() {
        let body = vec![7u8; 200_000];
        let url = serve("application/octet-stream", &body);
        let dir = std::env::temp_dir().join(format!("skyward-web-{}", std::process::id()));
        let path = dir.join("saved.bin");
        let agent = ureq::Agent::new_with_defaults();
        let mut seen = Vec::new();
        let size = save(&agent, &url, &path, &AtomicBool::new(false), &mut |p| seen.push(p)).unwrap();
        assert_eq!(size, body.len() as u64);
        assert_eq!(fs::read(&path).unwrap(), body);
        assert_eq!(seen.last().unwrap().done, body.len() as u64);
        let _ = fs::remove_dir_all(&dir);
    }
}
