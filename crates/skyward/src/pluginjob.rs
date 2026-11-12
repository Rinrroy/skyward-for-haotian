//! Fetching a plugin: the published index, and the image it names.
//!
//! The index is the one small file that says what there is to download. It is
//! read over the network, so it is held to the same rules a manifest on the
//! phone is — see [`skyward_ui::plugin::index`] — and the image it names is
//! checked against its digest before it is given its name.
//!
//! The work happens on a thread of its own because an image is hundreds of
//! megabytes: the pages must keep drawing, and the phone must be able to say
//! stop. What the thread learns is told through a channel the loop drains, so
//! nothing here draws or decides.

use crate::wlan;
use anyhow::{bail, Context, Result};
use skyward_net::download::{self, Progress, Sum};
use skyward_net::http::{self, Servers};
use skyward_ui::lang::Lang;
use skyward_ui::plugin::{self, Offered};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Where the index is published. A release asset, so the address stays the
/// same from one version to the next; the mirror is tried when GitHub does
/// not answer, which is what a phone in the mainland needs.
pub const INDEX: &str = "https://github.com/Rinrroy/skyward-plugins/releases/latest/download/plugins.json";

/// The most an index may be. It names plugins and their images, not the
/// images themselves.
const INDEX_MOST: u64 = 256 * 1024;

/// How often progress is told. A page redrawn sixty times a second does not
/// need a number sixty times a second, and every one of these crosses a
/// channel.
const EVERY: Duration = Duration::from_millis(250);

/// What the thread has to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Looking up what there is to download.
    Asking,
    /// The image is coming: how much of it, and how fast.
    Coming { done: u64, total: u64, rate: u64 },
    /// It is done, or it is not, and this is the sentence to draw.
    Ended(Ended),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    /// Installed as this folder name, with this many bytes.
    Installed { name: String, size: u64 },
    /// Asked to stop, and stopped.
    Stopped,
    /// Nothing to install, or nothing that could be installed.
    Failed(String),
}

/// A download under way.
pub struct Worker {
    told: Receiver<Event>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    /// Ask for the browser, and start it coming.
    pub fn start(store: PathBuf, servers: Servers, lang: Lang) -> Worker {
        let (tell, told) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let halting = stop.clone();
        let saying = tell.clone();
        let thread = thread::Builder::new()
            .name("plugin".into())
            .spawn(move || {
                let ended = match fetch(&store, &servers, &halting, &saying, lang) {
                    Ok(Some((name, size))) => Ended::Installed { name, size },
                    Ok(None) => Ended::Stopped,
                    Err(e) => Ended::Failed(format!("{e:#}")),
                };
                let _ = saying.send(Event::Ended(ended));
            });
        // A thread that could not be made says so at once rather than leaving
        // the page saying it is looking forever.
        let thread = match thread {
            Ok(handle) => Some(handle),
            Err(e) => {
                let _ = tell.send(Event::Ended(Ended::Failed(format!("start the download: {e}"))));
                None
            }
        };
        Worker { told, stop, thread }
    }

    /// Everything the thread has said since last asked.
    pub fn events(&self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(event) = self.told.try_recv() {
            out.push(event);
        }
        out
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Read a small file over the network, the direct address first and the
/// mirrors after it. The first answer that is a 200 is the answer.
fn small(servers: &Servers, url: &str, most: u64) -> Result<String> {
    let mut refused = Vec::new();
    for candidate in skyward_net::mirror::candidates(url) {
        let agent = http::agent(servers.clone());
        match agent.get(&candidate).call() {
            Ok(response) => {
                if response.status().as_u16() != 200 {
                    refused.push(format!("{candidate}: {}", response.status().as_u16()));
                    continue;
                }
                let mut body = std::io::Read::take(response.into_body().into_reader(), most + 1);
                let mut text = String::new();
                std::io::Read::read_to_string(&mut body, &mut text).context("read the answer")?;
                if text.len() as u64 > most {
                    bail!("the answer is larger than {most} bytes");
                }
                return Ok(text);
            }
            Err(e) => refused.push(format!("{candidate}: {e}")),
        }
    }
    bail!("no address answered ({})", refused.join("; "))
}

/// The whole job. `Ok(Some(...))` installed it, `Ok(None)` was stopped.
fn fetch(store: &Path, servers: &Servers, stop: &AtomicBool, tell: &mpsc::Sender<Event>, lang: Lang) -> Result<Option<(String, u64)>> {
    let _ = tell.send(Event::Asking);
    let text = small(servers, INDEX, INDEX_MOST).with_context(|| lang.pick(["读不到插件列表", "讀不到外掛列表", "the plugin list can't be read"]))?;
    let offered: Vec<Offered> = plugin::index(&text).map_err(|why| anyhow::anyhow!(why))?;
    // The one the page offers. An index that names several is one this
    // program will grow into; today it offers the browser.
    let Some(offered) = offered.into_iter().next() else {
        bail!("{}", lang.pick(["插件列表是空的", "外掛列表是空的", "the plugin list is empty"]));
    };
    let payload = offered.plugin.payload.clone().context("the plugin offers no image")?;
    let dir = store.join(&offered.name);
    fs::create_dir_all(&dir).with_context(|| format!("make {}", dir.display()))?;
    let image = crate::plugins::image(&dir, &payload);

    let mut last = Instant::now() - EVERY;
    let mut samples: Vec<(Instant, u64)> = Vec::new();
    let mut progress = |p: Progress| {
        if let Progress::Downloading { done, total } = p {
            let now = Instant::now();
            samples.push((now, done));
            samples.retain(|(at, _)| now.duration_since(*at) <= Duration::from_secs(3));
            let rate = match (samples.first(), samples.last()) {
                (Some((t0, d0)), Some((t1, d1))) if t1 > t0 => ((d1.saturating_sub(*d0)) as f64 / t1.duration_since(*t0).as_secs_f64()) as u64,
                _ => 0,
            };
            if last.elapsed() >= EVERY {
                last = Instant::now();
                let _ = tell.send(Event::Coming { done, total, rate });
            }
        }
    };
    let sum = Sum::Sha256(payload.sha256.clone());
    let mut refused = Vec::new();
    for candidate in skyward_net::mirror::candidates(&payload.url) {
        if stop.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let agent = http::agent(servers.clone());
        match download::fetch(&agent, &candidate, &image, &sum, stop, &mut progress) {
            Ok(()) => {
                // The manifest is written last, so a plugin that is only half
                // downloaded is not a plugin the page lists.
                let manifest = dir.join(plugin::FILE);
                let part = manifest.with_extension("json.part");
                fs::write(&part, &offered.text).with_context(|| format!("write {}", part.display()))?;
                fs::rename(&part, &manifest).with_context(|| format!("name {}", manifest.display()))?;
                let size = fs::metadata(&image).map(|m| m.len()).unwrap_or(payload.size);
                let _ = skyward_sys::kmsg(&format!("the browser is installed at {} ({size} bytes)", image.display()));
                return Ok(Some((offered.name, size)));
            }
            Err(e) => {
                if e.is::<skyward_net::Stopped>() || stop.load(Ordering::Relaxed) {
                    return Ok(None);
                }
                let _ = skyward_sys::kmsg(&format!("downloading the browser from {}: {e:#}", candidate));
                refused.push(format!("{e:#}"));
            }
        }
    }
    bail!("no address would send the image ({})", refused.join("; "))
}

/// Where the images are installed, for the worker that needs to know before
/// the page does.
pub fn store() -> PathBuf {
    crate::plugins::store()
}

/// The DNS servers a Wi-Fi join gave, as the agent wants them.
pub fn servers(wlan: &Option<wlan::Worker>) -> Servers {
    wlan.as_ref().map(|w| w.servers()).unwrap_or_default()
}

