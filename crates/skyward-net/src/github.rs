//! Skyward's own releases on GitHub: which is the newest, and the image in
//! it for this phone, as the release workflow names it.

use crate::mirror;
use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;

/// The repository this fork's releases are published in. Upstream's own
/// `Rinrroy/skyward-for-haotian` builds for many phones; this fork is the Xiaomi 15 Pro
/// alone, so it looks at its own releases and nowhere else.
pub const REPOSITORY: &str = "Rinrroy/skyward-for-haotian";

/// Where GitHub answers which release of this fork is the newest.
pub const LATEST: &str = "https://api.github.com/repos/Rinrroy/skyward-for-haotian/releases/latest";

/// The version of GitHub's API the answer is read as.
const API_VERSION: &str = "2022-11-28";

/// The newest release, as the build of Skyward that asked sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Newest {
    /// Its version, such as 0.1.4.
    pub version: String,
    /// It is newer than the build that asked.
    pub newer: bool,
    /// Its build for the phone; none when it has none.
    pub build: Option<Build>,
    /// The mirror that answered in GitHub's place, such as gh-proxy.com; none
    /// where GitHub itself answered.
    pub via: Option<String>,
}

/// A phone's build in a release, which is Skyward's image for the phone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Build {
    /// The image's own name, such as
    /// skyward-v0.1.4-ishtar-OS3.0.308.0.WMACNXM.img.
    pub name: String,
    /// Where GitHub's API hands it out, which [`locate`] asks for where the
    /// file itself is.
    pub url: String,
    /// The release's own link to it on github.com.
    pub link: String,
    pub size: u64,
    /// Its SHA-256 as GitHub gives it, in hex.
    pub sha256: String,
}

impl Build {
    /// Where to download the image from, in the order to try: the file's own
    /// short-lived address, where GitHub's API gave one, then the release's
    /// link on github.com; then the mirrors of each of those, for the
    /// networks that reach neither. GitHub's own addresses come first, so a
    /// network that reaches GitHub never goes through a mirror at all, and
    /// the digest the answer gave still gates whatever arrives from one.
    pub fn sources(&self, located: Option<String>) -> Vec<String> {
        let direct: Vec<String> = located.into_iter().chain([self.link.clone()]).collect();
        let mut out = direct.clone();
        for url in &direct {
            // The address itself is the first candidate and is already here:
            // its mirrors follow it.
            for candidate in mirror::candidates(url).into_iter().skip(1) {
                if !out.contains(&candidate) {
                    out.push(candidate);
                }
            }
        }
        out
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    url: String,
    name: String,
    size: u64,
    /// The digest GitHub took of the file as it was uploaded, such as
    /// `sha256:` and the SHA-256 in hex.
    digest: Option<String>,
    browser_download_url: String,
}

/// Why GitHub turned a question down, as it says.
#[derive(Deserialize)]
struct Message {
    message: String,
}

/// The newest release that `answer`, GitHub's answer at [`LATEST`], tells
/// of, and its build for the phone `codename` names, as the build of version
/// `running` sees it.
pub fn newest(answer: &str, codename: &str, running: &str) -> Result<Newest> {
    let release: Release = serde_json::from_str(answer).context("read GitHub's answer")?;
    let tag = release.tag_name;
    let version = tag.strip_prefix('v').with_context(|| format!("the newest release is tagged {tag}, which is not v and a version"))?;
    let newest = semver::Version::parse(version).with_context(|| format!("the newest release is tagged {tag}"))?;
    let this = semver::Version::parse(running).with_context(|| format!("this build's version is {running}"))?;
    // The image is named skyward-, the tag, the phone's codename, and the
    // stock build it is made on, and is put on the release as it is - not
    // zipped, so the phone downloads what it writes.
    let start = format!("skyward-{tag}-{codename}-");
    let build = match release.assets.into_iter().find(|a| a.name.starts_with(&start) && a.name.ends_with(".img")) {
        None => None,
        Some(a) => {
            let sha256 = a.digest.as_deref().and_then(|d| d.strip_prefix("sha256:")).with_context(|| format!("GitHub gives no SHA-256 of {}", a.name))?;
            ensure!(sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit()), "GitHub gives {} the SHA-256 {sha256}", a.name);
            Some(Build { sha256: sha256.to_lowercase(), name: a.name, url: a.url, link: a.browser_download_url, size: a.size })
        }
    };
    Ok(Newest { version: version.to_string(), newer: newest > this, build, via: None })
}

/// Ask GitHub for Skyward's newest release, and its build for the phone
/// `codename` names, as the build of version `running`: of GitHub's own
/// address, and then of each mirror of it, while the network allows them.
pub fn ask(agent: &ureq::Agent, codename: &str, running: &str) -> Result<Newest> {
    ask_candidates(agent, mirror::candidates(LATEST), codename, running)
}

/// Ask each of `urls` in turn for Skyward's newest release, and take the
/// first answer that gives one. Where none does, the failure told is the
/// first, GitHub's own, so that it names api.github.com rather than a mirror
/// that was tried after it.
fn ask_candidates<I>(agent: &ureq::Agent, urls: I, codename: &str, running: &str) -> Result<Newest>
where
    I: IntoIterator<Item = String>,
{
    let mut first = None;
    for url in urls {
        match ask_at(agent, &url, codename, running) {
            Ok(mut newest) => {
                newest.via = if url == LATEST { None } else { mirror::host(&url) };
                return Ok(newest);
            }
            Err(e) => {
                let _ = first.get_or_insert(e);
            }
        }
    }
    match first {
        Some(e) => Err(e),
        None => bail!("ask {LATEST}"),
    }
}

/// Ask `url`, GitHub's own address or a mirror of it, for Skyward's newest
/// release, and its build for the phone `codename` names, as the build of
/// version `running`.
fn ask_at(agent: &ureq::Agent, url: &str, codename: &str, running: &str) -> Result<Newest> {
    let mut response = agent
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", API_VERSION)
        .header("User-Agent", format!("Skyward/{running}"))
        .call()
        .with_context(|| format!("ask {url}"))?;
    let status = response.status();
    let body = response.body_mut().read_to_string().with_context(|| format!("read the answer of {url}"))?;
    if !status.is_success() {
        bail!(refusal(status, &body));
    }
    newest(&body, codename, running)
}

/// Where the file a build's `url` hands out is now, as GitHub's API sends the
/// build of version `running` that asks for it on: a short-lived address of
/// the file itself, away from github.com. A mirror of the API may send it on
/// to an address of its own, and that is taken too.
pub fn locate(agent: &ureq::Agent, url: &str, running: &str) -> Result<String> {
    locate_candidates(agent, mirror::candidates(url), running)
}

/// Ask each of `urls` in turn where the file it hands out is, and take the
/// first address given. Where none gives one, the failure told is the first,
/// GitHub's own.
fn locate_candidates<I>(agent: &ureq::Agent, urls: I, running: &str) -> Result<String>
where
    I: IntoIterator<Item = String>,
{
    let mut first = None;
    for url in urls {
        match locate_at(agent, &url, running) {
            Ok(location) => return Ok(location),
            Err(e) => {
                let _ = first.get_or_insert(e);
            }
        }
    }
    match first {
        Some(e) => Err(e),
        None => bail!("no address was given to ask where the build's file is"),
    }
}

/// Ask `url`, GitHub's own address or a mirror of it, where the file it hands
/// out is now.
fn locate_at(agent: &ureq::Agent, url: &str, running: &str) -> Result<String> {
    let mut response = agent
        .get(url)
        .header("Accept", "application/octet-stream")
        .header("X-GitHub-Api-Version", API_VERSION)
        .header("User-Agent", format!("Skyward/{running}"))
        .config()
        .max_redirects(0)
        .build()
        .call()
        .with_context(|| format!("ask {url}"))?;
    let status = response.status();
    if status.is_redirection() {
        let location = response.headers().get(ureq::http::header::LOCATION).with_context(|| format!("GitHub answered {status}, and names no address"))?;
        let location = location.to_str().context("GitHub names an address that is not text")?;
        // A prefix proxy sends the download on to its own address of the file,
        // or to the file itself: whatever absolute https address it names is
        // where to go on from.
        ensure!(absolute_https(location), "GitHub answers {status}, and names {location}, which is no address to download the file from");
        return Ok(location.to_string());
    }
    ensure!(!status.is_success(), "GitHub sends the file itself in its answer, at no address of its own");
    let body = response.body_mut().read_to_string().with_context(|| format!("read the answer of {url}"))?;
    bail!(refusal(status, &body))
}

/// Whether `url` is an absolute https address, as an address to download a
/// file from must be: one that names a host, and asks for it over https.
fn absolute_https(url: &str) -> bool {
    match url.strip_prefix("https://") {
        Some(rest) => !rest.is_empty() && !rest.starts_with('/'),
        None => false,
    }
}

/// Why GitHub turned a question down, as it says, as when too many questions
/// came from the address that asked.
fn refusal(status: ureq::http::StatusCode, body: &str) -> String {
    match serde_json::from_str::<Message>(body) {
        Ok(m) => format!("GitHub answered {status}: {}", m.message),
        Err(_) => format!("GitHub answered {status}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What GitHub answered of the newest release on 2026-11-07, with the
    /// parts that do not matter here left out.
    const ANSWERED_013: &str = r#"{"url":"https://api.github.com/repos/Rinrroy/skyward-for-haotian/releases/437365758","tag_name":"v0.1.3","name":"v0.1.3","draft":false,"prerelease":false,"assets":[{"url":"https://api.github.com/repos/Rinrroy/skyward-for-haotian/releases/assets/719751468","id":719751468,"name":"SHA256SUMS","label":"","content_type":"application/octet-stream","state":"uploaded","size":2337,"digest":"sha256:052f9ca757baeb2eac78c8e546bcc54178f6bb9bf9e5bde53fa81b3f48448957","browser_download_url":"https://github.com/Rinrroy/skyward-for-haotian/releases/download/v0.1.3/SHA256SUMS"},{"url":"https://api.github.com/repos/Rinrroy/skyward-for-haotian/releases/assets/719751598","id":719751598,"name":"skyward-v0.1.3-ishtar-OS3.0.308.0.WMACNXM.img","label":"Xiaomi 13 Ultra (ishtar)","content_type":"application/octet-stream","state":"uploaded","size":42739608,"digest":"sha256:62b66f7d13a0eb6c9af3359b5ec6f9c223fd9a86b1f8abab3e9d2646cb2ce534","browser_download_url":"https://github.com/Rinrroy/skyward-for-haotian/releases/download/v0.1.3/skyward-v0.1.3-ishtar-OS3.0.308.0.WMACNXM.img"},{"url":"https://api.github.com/repos/Rinrroy/skyward-for-haotian/releases/assets/719751747","id":719751747,"name":"skyward-v0.1.3-nezha-OS4.0.4.0.XPACNXM.img","label":"Xiaomi 17 Ultra (nezha)","content_type":"application/octet-stream","state":"uploaded","size":43145196,"digest":"sha256:9f6165079529418d8f6635beddfdd9abf9a70c89d69eb467a9922b0db69bfaf1","browser_download_url":"https://github.com/Rinrroy/skyward-for-haotian/releases/download/v0.1.3/skyward-v0.1.3-nezha-OS4.0.4.0.XPACNXM.img"}]}"#;

    #[test]
    fn the_newest_release_gives_the_build_for_the_phone() {
        let n = newest(ANSWERED_013, "ishtar", "0.1.2").unwrap();
        assert_eq!(n.version, "0.1.3");
        assert!(n.newer);
        assert_eq!(
            n.build,
            Some(Build {
                name: "skyward-v0.1.3-ishtar-OS3.0.308.0.WMACNXM.img".into(),
                url: "https://api.github.com/repos/Rinrroy/skyward-for-haotian/releases/assets/719751598".into(),
                link: "https://github.com/Rinrroy/skyward-for-haotian/releases/download/v0.1.3/skyward-v0.1.3-ishtar-OS3.0.308.0.WMACNXM.img".into(),
                size: 42739608,
                sha256: "62b66f7d13a0eb6c9af3359b5ec6f9c223fd9a86b1f8abab3e9d2646cb2ce534".into(),
            })
        );
        assert_eq!(newest(ANSWERED_013, "nezha", "0.1.2").unwrap().build.unwrap().size, 43145196);
    }

    #[test]
    fn an_image_is_downloaded_from_where_github_sends_it_then_from_its_link_then_from_a_mirror_of_each() {
        let build = newest(ANSWERED_013, "ishtar", "0.1.2").unwrap().build.unwrap();
        let link = "https://github.com/Rinrroy/skyward-for-haotian/releases/download/v0.1.3/skyward-v0.1.3-ishtar-OS3.0.308.0.WMACNXM.img";
        let located = "https://release-assets.githubusercontent.com/a?sig=1";
        // Every address GitHub gave comes before any mirror of one: a network
        // that reaches GitHub does not go through a mirror at all.
        assert_eq!(
            build.sources(Some(located.into())),
            [
                located.to_string(),
                link.to_string(),
                format!("https://gh-proxy.com/{located}"),
                format!("https://ghproxy.net/{located}"),
                format!("https://ghfast.top/{located}"),
                format!("https://gh-proxy.com/{link}"),
                format!("https://ghproxy.net/{link}"),
                format!("https://ghfast.top/{link}"),
            ]
        );
        assert_eq!(
            build.sources(None),
            [link.to_string(), format!("https://gh-proxy.com/{link}"), format!("https://ghproxy.net/{link}"), format!("https://ghfast.top/{link}")],
            "where GitHub's API gave no address"
        );
    }

    /// A server on this computer that answers one request with `answer`,
    /// and gives the request it had.
    fn server(answer: String) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/repos/Rinrroy/skyward-for-haotian/releases/assets/719751598", listener.local_addr().unwrap());
        let held = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 512];
            while !request.ends_with(b"\r\n\r\n") {
                let n = s.read(&mut chunk).unwrap();
                request.extend_from_slice(&chunk[..n]);
            }
            s.write_all(answer.as_bytes()).unwrap();
            String::from_utf8(request).unwrap()
        });
        (url, held)
    }

    #[test]
    fn github_s_api_sends_the_image_on_to_where_the_file_is() {
        let agent = crate::http::agent(Default::default());
        let sent_on = "HTTP/1.1 302 Found\r\nLocation: https://release-assets.githubusercontent.com/a?sig=1\r\nContent-Length: 0\r\n\r\n";
        let (url, held) = server(sent_on.into());
        assert_eq!(locate(&agent, &url, "0.1.2").unwrap(), "https://release-assets.githubusercontent.com/a?sig=1", "the address, not followed");
        let request = held.join().unwrap().to_lowercase();
        assert!(request.contains("accept: application/octet-stream\r\n"), "asked for the file itself: {request}");
        assert!(request.contains("user-agent: skyward/0.1.2\r\n"), "{request}");
        let why = r#"{"message":"API rate limit exceeded for 203.0.113.7"}"#;
        let (url, _) = server(format!("HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{why}", why.len()));
        assert_eq!(locate(&agent, &url, "0.1.2").unwrap_err().to_string(), "GitHub answered 403 Forbidden: API rate limit exceeded for 203.0.113.7");
        let (url, _) = server("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nPK".into());
        assert!(locate(&agent, &url, "0.1.2").is_err(), "the file itself, with no address to download it from again");
    }

    #[test]
    fn the_answer_of_a_mirror_is_taken_after_github_s_own_is_turned_down() {
        let agent = crate::http::agent(Default::default());
        let why = r#"{"message":"API rate limit exceeded for 203.0.113.7"}"#;
        let refused = || format!("HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{why}", why.len());
        let (direct, _) = server(refused());
        let (mirror, held) = server(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{ANSWERED_013}", ANSWERED_013.len()));
        let answered = ask_candidates(&agent, [direct, mirror.clone()], "ishtar", "0.1.2").unwrap();
        assert_eq!(answered.version, "0.1.3", "the mirror's answer, after GitHub's own was turned down");
        assert!(answered.newer);
        assert_eq!(answered.via.as_deref(), mirror.split('/').nth(2), "the mirror that answered is named by its host");
        let request = held.join().unwrap().to_lowercase();
        assert!(request.contains("accept: application/vnd.github+json\r\n"), "the mirror is asked as GitHub is: {request}");
        // Where no mirror answers either, the failure told is GitHub's own.
        let (direct, _) = server(refused());
        let (mirror, _) = server(refused());
        let e = ask_candidates(&agent, [direct, mirror], "ishtar", "0.1.2").unwrap_err();
        assert_eq!(e.to_string(), "GitHub answered 403 Forbidden: API rate limit exceeded for 203.0.113.7", "the first failure, which names api.github.com");
    }

    #[test]
    fn an_address_a_mirror_sends_the_file_on_to_is_taken_and_one_that_names_no_host_is_not() {
        let agent = crate::http::agent(Default::default());
        let rewritten = "https://gh-proxy.com/https://release-assets.githubusercontent.com/a?sig=1";
        let (url, _) = server(format!("HTTP/1.1 302 Found\r\nLocation: {rewritten}\r\nContent-Length: 0\r\n\r\n"));
        assert_eq!(locate_candidates(&agent, [url], "0.1.2").unwrap(), rewritten, "a proxy's own address of the file itself");
        let (url, _) = server("HTTP/1.1 302 Found\r\nLocation: /a?sig=1\r\nContent-Length: 0\r\n\r\n".into());
        assert!(locate_candidates(&agent, [url], "0.1.2").is_err(), "an address with no host of its own is no address to download from");
    }

    #[test]
    fn a_release_is_newer_only_than_an_older_build() {
        assert!(!newest(ANSWERED_013, "ishtar", "0.1.3").unwrap().newer, "the same version");
        assert!(!newest(ANSWERED_013, "ishtar", "0.2.0").unwrap().newer, "a build ahead of every release");
        assert!(newest(ANSWERED_013, "ishtar", "0.1.3-rc.1").unwrap().newer, "a build before the release it led to");
        assert!(newest(ANSWERED_013, "ishtar", "0.1").is_err(), "a version this build cannot have");
    }

    #[test]
    fn a_phone_is_found_by_its_whole_codename() {
        assert_eq!(newest(ANSWERED_013, "fuxi", "0.1.2").unwrap().build, None, "a phone the release has no build for");
        assert_eq!(newest(ANSWERED_013, "ish", "0.1.2").unwrap().build, None, "the start of another phone's codename");
        assert_eq!(newest(ANSWERED_013, "SHA256SUMS", "0.1.2").unwrap().build, None);
    }

    #[test]
    fn a_build_needs_its_sha256_and_a_release_its_version() {
        let unsummed = ANSWERED_013.replace("\"digest\":\"sha256:62b66f7d13a0eb6c9af3359b5ec6f9c223fd9a86b1f8abab3e9d2646cb2ce534\"", "\"digest\":null");
        let e = newest(&unsummed, "ishtar", "0.1.2").unwrap_err();
        assert_eq!(e.to_string(), "GitHub gives no SHA-256 of skyward-v0.1.3-ishtar-OS3.0.308.0.WMACNXM.img");
        assert!(newest(&unsummed, "nezha", "0.1.2").is_ok(), "another phone's build has its own");
        let untagged = ANSWERED_013.replace("\"tag_name\":\"v0.1.3\"", "\"tag_name\":\"nightly\"");
        assert!(newest(&untagged, "ishtar", "0.1.2").is_err());
        assert!(newest("{}", "ishtar", "0.1.2").is_err());
    }
}
