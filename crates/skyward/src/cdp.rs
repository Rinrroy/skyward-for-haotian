//! A small Chrome DevTools Protocol client for the persistent full-mode
//! Chromium. It speaks just enough WebSocket and CDP to keep one page alive
//! and send it real touch, pinch and text input.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

const PORT: u16 = 9222;
/// The page lays out as a phone does, this many CSS pixels across; the height
/// follows the box the recovery draws the frame in, so that what Chromium
/// paints is what the box shows and neither is stretched to fit the other.
const CSS_W: f64 = 412.0;
/// The frame the recovery asks for when it has not said how big its box is:
/// a phone viewport at three device pixels to the CSS one.
const FALLBACK_CSS_H: f64 = 915.0;
const FALLBACK_SCALE: f64 = 3.0;
/// A move of a drag is given up on after this long: a page busy running a
/// script of its own would otherwise hold every move of the drag behind it.
const SCROLL_WITHIN: Duration = Duration::from_millis(600);
/// Move what is under the middle of the frame by `__PX__` CSS pixels: the
/// nearest thing that can scroll, or the page itself when nothing between the
/// finger and the page can.
const SCROLL: &str = "(()=>{\
const px=__PX__;\
const cx=Math.round(innerWidth/2),cy=Math.round(innerHeight/2);\
let e=document.elementFromPoint(cx,cy);\
while(e&&e!==document.body&&e!==document.documentElement){\
const s=getComputedStyle(e);\
if(e.scrollHeight-e.clientHeight>2&&/(auto|scroll|overlay)/.test(s.overflowY)){e.scrollTop+=px;return 1}\
e=e.parentElement}\
const d=document.scrollingElement||document.documentElement;\
d.scrollTop+=px;return 1})()";

pub struct Cdp {
    stream: TcpStream,
    next: u64,
    /// The viewport being drawn, in CSS pixels: with the page's own scale it
    /// is the frame's size in device pixels, so the recovery never resizes one.
    view: (f64, f64),
}

impl Cdp {
    /// Attach to the page and tell it to draw for a box `pixels` device pixels
    /// across and tall. The page keeps a phone's layout width, and the height
    /// it keeps is the one the box has, so scrolling happens in the page: the
    /// recovery's own page never moves under it.
    pub fn connect(pixels: (u32, u32)) -> Result<Cdp> {
        let (w, h) = (pixels.0.max(1) as f64, pixels.1.max(1) as f64);
        // A box narrower than a phone's own width would ask for a scale below
        // one, which draws a page smaller than it reads: the page keeps the
        // device pixels it was written for and the frame is captured at them.
        let scale = (w / CSS_W).clamp(1.0, 4.0);
        // The height has to be a whole number of CSS pixels: Chromium takes
        // this one as an integer and refuses the whole command otherwise,
        // which is a page that never attaches at all.
        let css = (CSS_W, (h / scale).max(120.0).round());
        Cdp::attach(css, scale)
    }

    /// Attach without a box to draw for, which is what a page already talking
    /// to Chromium does when the recovery has not said how big it is.
    pub fn connect_default() -> Result<Cdp> {
        Cdp::attach((CSS_W, FALLBACK_CSS_H), FALLBACK_SCALE)
    }

    fn attach(css: (f64, f64), scale: f64) -> Result<Cdp> {
        let deadline = Instant::now() + Duration::from_secs(15);
        let ws = loop {
            // Chromium answers its page list as soon as it listens, which is a
            // moment before it has a page in it: what is looked for is the
            // page, not the answer, so this waits for the one it opened rather
            // than giving up on the first list that came back empty.
            let list = match http_get("/json/list").ok().and_then(|list| serde_json::from_str::<Value>(&list).ok()) {
                Some(list) => list,
                None if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
                None => return Err(anyhow!("wait for Chromium's debugging port")).context("attach to the page"),
            };
            let found = list
                .as_array()
                .and_then(|pages| pages.iter().find(|page| page["type"] == "page"))
                .and_then(|page| page["webSocketDebuggerUrl"].as_str())
                .map(str::to_owned);
            match found {
                Some(ws) => break ws,
                None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(200)),
                None => return Err(anyhow!("Chromium lists no page to attach to")).context("attach to the page"),
            }
        };
        let path = ws.strip_prefix("ws://127.0.0.1:9222").or_else(|| ws.strip_prefix("ws://localhost:9222")).context("unexpected debugging socket")?;
        let mut stream = TcpStream::connect(("127.0.0.1", PORT)).context("connect to Chromium")?;
        stream.set_read_timeout(Some(Duration::from_secs(20)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        let key = "U2t5d2FyZC1icm93c2VyIQ==";
        write!(stream, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n")?;
        let headers = read_headers(&mut stream)?;
        if !headers.starts_with("HTTP/1.1 101") && !headers.starts_with("HTTP/1.0 101") {
            bail!("Chromium refused the debugging socket: {}", headers.lines().next().unwrap_or(""));
        }
        let mut cdp = Cdp { stream, next: 1, view: css };
        cdp.call("Page.enable", json!({}))?;
        cdp.call(
            "Emulation.setDeviceMetricsOverride",
            json!({"width": css.0, "height": css.1, "deviceScaleFactor": scale, "mobile": true}),
        )?;
        cdp.call("Emulation.setTouchEmulationEnabled", json!({"enabled": true, "maxTouchPoints": 5}))?;
        Ok(cdp)
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.send(method, params)?;
        loop {
            let payload = read_frame(&mut self.stream)?;
            let answer: Value = match serde_json::from_slice(&payload) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if answer["id"].as_u64() != Some(id) {
                continue;
            }
            if let Some(error) = answer.get("error") {
                bail!("{method}: {error}");
            }
            return Ok(answer.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Ask the page for something and give it `within` to answer: a page that
    /// is still loading holds its own scripts for seconds, and a gesture that
    /// waited that long would feel stuck. An answer that comes too late is
    /// skipped by the next call, since answers say which ask they belong to.
    pub fn call_within(&mut self, method: &str, params: Value, within: Duration) -> Result<Value> {
        self.stream.set_read_timeout(Some(within))?;
        let answer = self.call(method, params);
        self.stream.set_read_timeout(Some(Duration::from_secs(20)))?;
        answer
    }

    /// Ask the page for something without waiting for the answer, which is what
    /// a navigation is: it is the page that takes the time, not the asking, and
    /// waiting would hold every gesture behind it.
    fn send(&mut self, method: &str, params: Value) -> Result<u64> {
        let id = self.next;
        self.next += 1;
        let message = json!({"id": id, "method": method, "params": params});
        write_frame(&mut self.stream, message.to_string().as_bytes())?;
        Ok(id)
    }

    pub fn navigate(&mut self, url: &str) -> Result<()> {
        self.send("Page.navigate", json!({"url": url}))?;
        Ok(())
    }

    pub fn reload(&mut self) -> Result<()> {
        self.call("Page.reload", json!({}))?;
        std::thread::sleep(Duration::from_millis(900));
        Ok(())
    }

    pub fn touch(&mut self, x: u32, y: u32) -> Result<()> {
        let point = json!([{"x": x, "y": y, "id": 1, "radiusX": 2, "radiusY": 2, "force": 1.0}]);
        self.call("Input.dispatchTouchEvent", json!({"type": "touchStart", "touchPoints": point}))?;
        std::thread::sleep(Duration::from_millis(50));
        self.call("Input.dispatchTouchEvent", json!({"type": "touchEnd", "touchPoints": []}))?;
        // A page answers a tap in a frame or two of its own; waiting longer
        // than that only holds the picture back.
        std::thread::sleep(Duration::from_millis(180));
        Ok(())
    }

    /// Touch the page where a finger touched the frame, `x` and `y` being
    /// thousandths of the frame's width and height: what is under that point
    /// of the picture is what is under the finger.
    pub fn tap(&mut self, x: u32, y: u32) -> Result<()> {
        let px = (x as f64 * self.view.0 / 1000.0).clamp(0.0, (self.view.0 - 1.0).max(0.0));
        let py = (y as f64 * self.view.1 / 1000.0).clamp(0.0, (self.view.1 - 1.0).max(0.0));
        self.touch(px.round() as u32, py.round() as u32)
    }

    /// Scroll the page under the finger by `dy` thousandths of the frame's
    /// height, moving whatever is under the middle of the frame: the page
    /// itself, or the scrollable box the page puts there.
    ///
    /// This is asked of the page rather than done with a synthesized gesture
    /// because nothing else moved this page where the finger went: a
    /// synthesized scroll gesture answered only after the whole gesture had
    /// been carried out and then scrolled nothing, a wheel event scrolled
    /// nothing at all, and a touch from down to up scrolled about half of what
    /// it was asked for — and, being a touch, it left the page thrown on by
    /// its own momentum when the finger lifted. A page told where to go goes
    /// exactly there, at once, and stays.
    pub fn scroll(&mut self, dy: i32) -> Result<()> {
        let px = (f64::from(dy) * self.view.1 / 1000.0).round();
        if px == 0.0 {
            return Ok(());
        }
        let code = SCROLL.replace("__PX__", &format!("{px}"));
        // A page busy running a script of its own holds this call up for as
        // long as that script runs, so the move is given up on rather than
        // waited for: the next move of the drag brings the page down anyway.
        self.call_within(
            "Runtime.evaluate",
            json!({"expression": code, "returnByValue": true}),
            SCROLL_WITHIN,
        )?;
        Ok(())
    }

    /// Zoom the page in or out about the middle of the frame.
    pub fn pinch(&mut self, scale: f64) -> Result<()> {
        self.call(
            "Input.synthesizePinchGesture",
            json!({
                "x": (self.view.0 / 2.0).round(),
                "y": (self.view.1 / 2.0).round(),
                "scaleFactor": scale,
                "relativeSpeed": 800,
            }),
        )?;
        std::thread::sleep(Duration::from_millis(400));
        Ok(())
    }

    pub fn insert_text(&mut self, text: &str) -> Result<()> {
        self.call("Input.insertText", json!({"text": text}))?;
        std::thread::sleep(Duration::from_millis(300));
        Ok(())
    }

    pub fn focus_is_text(&mut self) -> bool {
        let answer = self.call_within(
            "Runtime.evaluate",
            json!({"expression": "(()=>{const e=document.activeElement;return e?`${e.tagName}:${e.type||''}:${e.isContentEditable}`:''})()", "returnByValue": true}),
            // A page in the middle of loading can hold a script of its own for
            // ten seconds: the keyboard waits for the next tap rather than the
            // gesture waiting for the page.
            Duration::from_millis(700),
        );
        let Ok(answer) = answer else { return false };
        let value = answer["result"]["value"].as_str().unwrap_or("");
        value.starts_with("INPUT") || value.starts_with("TEXTAREA") || value.ends_with(":true")
    }

    /// The frame the recovery draws: the viewport itself, at the size the box
    /// it goes in has, so it is laid on the screen as it comes. The viewport is
    /// four hundred and twelve CSS pixels across and Chromium multiplies a
    /// clip by the page's own scale, so the clip is taken at one and comes back
    /// exactly as many device pixels wide as the box is: the recovery never
    /// stretches a frame to fit, which is what made the picture soft and slow.
    pub fn screenshot(&mut self) -> Result<Vec<u8>> {
        let answer = self.call(
            "Page.captureScreenshot",
            json!({
                "format": "jpeg",
                "quality": 60,
                // The frame is a picture of a page being dragged: encoding it
                // for size rather than for speed is what made a drag feel like
                // it was answered a moment late.
                "optimizeForSpeed": true,
                "fromSurface": true,
                "clip": {
                    "x": 0.0,
                    "y": 0.0,
                    "width": self.view.0,
                    "height": self.view.1,
                    "scale": 1.0
                }
            }),
        )?;
        let data = answer["data"].as_str().context("screenshot holds no data")?;
        base64::engine::general_purpose::STANDARD.decode(data).context("decode screenshot")
    }
}

fn http_get(path: &str) -> Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", PORT)).context("connect to Chromium")?;
    // Chromium writes the page list in one go and then holds the connection
    // open rather than closing it, so reading until the socket ends waits for
    // a close that never comes: the body's own length says when it is all in.
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(stream, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\nConnection: close\r\n\r\n")?;
    let mut bytes = Vec::new();
    let mut body = None;
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                bytes.extend_from_slice(&chunk[..read]);
                if body.is_none() {
                    body = bytes.windows(4).position(|window| window == b"\r\n\r\n").map(|at| at + 4);
                }
                if whole_answer(&bytes, body) {
                    break;
                }
            }
            // The wait running out after the body is in is not a failure: it
            // means Chromium is keeping the socket, which is what it does.
            Err(_) => break,
        }
    }
    let at = body.or_else(|| bytes.windows(4).position(|window| window == b"\r\n\r\n").map(|at| at + 4)).context("Chromium answered without a body")?;
    Ok(String::from_utf8_lossy(&bytes[at..]).into_owned())
}

/// Whether an HTTP answer that has been read so far holds all of its body.
fn whole_answer(bytes: &[u8], body: Option<usize>) -> bool {
    let Some(at) = body else { return false };
    let Some(length) = content_length(&bytes[..at]) else { return false };
    bytes.len() >= at + length
}

/// How many bytes the body of an HTTP answer takes, as its headers say.
fn content_length(headers: &[u8]) -> Option<usize> {    let text = String::from_utf8_lossy(headers);
    text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("content-length") {
            value.trim().parse().ok()
        } else {
            None
        }
    })
}

fn read_headers(stream: &mut TcpStream) -> Result<String> {
    let mut bytes = Vec::new();
    let mut one = [0u8; 1];
    while bytes.len() < 16384 {
        let n = stream.read(&mut one)?;
        if n == 0 {
            break;
        }
        bytes.push(one[0]);
        if bytes.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn write_frame(stream: &mut TcpStream, payload: &[u8]) -> Result<()> {
    let mut header = Vec::with_capacity(14);
    header.push(0x81);
    let mask = [0x53, 0x6b, 0x79, 0x77];
    if payload.len() < 126 {
        header.push(0x80 | payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        header.push(0x80 | 126);
        header.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        header.push(0x80 | 127);
        header.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    header.extend_from_slice(&mask);
    stream.write_all(&header)?;
    let mut masked = payload.to_vec();
    for (i, byte) in masked.iter_mut().enumerate() {
        *byte ^= mask[i % 4];
    }
    stream.write_all(&masked)?;
    stream.flush()?;
    Ok(())
}

fn read_frame(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut whole = Vec::new();
    loop {
        let mut two = [0u8; 2];
        stream.read_exact(&mut two)?;
        let fin = two[0] & 0x80 != 0;
        let opcode = two[0] & 0x0f;
        let masked = two[1] & 0x80 != 0;
        let mut len = (two[1] & 0x7f) as u64;
        if len == 126 {
            let mut bytes = [0u8; 2];
            stream.read_exact(&mut bytes)?;
            len = u16::from_be_bytes(bytes) as u64;
        } else if len == 127 {
            let mut bytes = [0u8; 8];
            stream.read_exact(&mut bytes)?;
            len = u64::from_be_bytes(bytes);
        }
        if len > 48 * 1024 * 1024 {
            bail!("Chromium sent an oversized frame");
        }
        let mask = if masked {
            let mut mask = [0u8; 4];
            stream.read_exact(&mut mask)?;
            Some(mask)
        } else {
            None
        };
        let mut payload = vec![0u8; len as usize];
        stream.read_exact(&mut payload)?;
        if let Some(mask) = mask {
            for (i, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[i % 4];
            }
        }
        if opcode == 0x8 {
            return Err(anyhow!("Chromium closed the debugging socket"));
        }
        if opcode == 0x9 || opcode == 0xA {
            continue;
        }
        whole.extend_from_slice(&payload);
        if fin {
            return Ok(whole);
        }
    }
}

#[allow(dead_code)]
fn accept_for(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}
